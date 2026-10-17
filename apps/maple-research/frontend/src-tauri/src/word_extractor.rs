use cfb::CompoundFile;
use office_oxide::doc::DocDocument;
use office_oxide::docx::DocxDocument;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use std::collections::HashSet;
use std::io::{self, Cursor, Read, Write};
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::{CompressionMethod, ZipArchive};

pub(crate) const WORD_PANIC_MESSAGE: &str = "Maple couldn't process this Word document because its parser stopped unexpectedly. The app is still running; try a different Word file.";

const WORD_READ_ERROR: &str =
    "Maple couldn't read this Word document. It may be damaged or use an unsupported Word feature.";
const WORD_PASSWORD_ERROR: &str =
    "This Word document is password-protected. Maple cannot read protected Word files yet.";
const WORD_EMPTY_ERROR: &str = "This Word document does not contain text Maple can read.";
const WORD_FORMAT_MISMATCH_ERROR: &str =
    "This file's contents do not match its DOC or DOCX file type.";
const WORD_COMPLEXITY_ERROR: &str =
    "This Word document is too complex for Maple to process safely.";

const CFB_MAGIC: &[u8; 8] = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1";
const MAX_CFB_STREAM_BYTES: u64 = 10 * 1024 * 1024;
const MAX_CFB_TOTAL_STREAM_BYTES: u64 = 24 * 1024 * 1024;
const MAX_OPC_PATH_BYTES: usize = 1_024;
const MAX_ZIP_CENTRAL_DIRECTORY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LEGACY_DOC_PIECES: usize = 65_536;

const CFB_MAX_REGULAR_SECTOR: u32 = 0xffff_fffa;
const CFB_END_OF_CHAIN: u32 = 0xffff_fffe;
const CFB_FREE_SECTOR: u32 = 0xffff_ffff;

const TEXT_ONLY_CONTENT_TYPES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;
const TEXT_ONLY_PACKAGE_RELS: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WordFileType {
    Doc,
    Docx,
}

#[derive(Debug, Clone, Copy)]
struct DocxLimits {
    max_entries: usize,
    max_entry_bytes: u64,
    max_total_bytes: u64,
    max_compression_ratio: u64,
    compression_ratio_allowance: u64,
    max_xml_depth: usize,
    max_xml_events: usize,
    max_model_elements: usize,
    max_table_depth: usize,
    max_attributes_per_element: usize,
    max_total_attributes: usize,
}

const DOCX_LIMITS: DocxLimits = DocxLimits {
    max_entries: 2_048,
    max_entry_bytes: 10 * 1024 * 1024,
    max_total_bytes: 40 * 1024 * 1024,
    max_compression_ratio: 200,
    compression_ratio_allowance: 1024 * 1024,
    max_xml_depth: 64,
    max_xml_events: 200_000,
    max_model_elements: 25_000,
    max_table_depth: 8,
    max_attributes_per_element: 1_024,
    max_total_attributes: 250_000,
};

pub(crate) fn extract_word_document(
    file_bytes: Vec<u8>,
    expected_type: WordFileType,
    max_extracted_text_bytes: usize,
) -> Result<String, String> {
    let text = match expected_type {
        WordFileType::Docx if is_zip(&file_bytes) => {
            let text_only_package = preflight_docx(&file_bytes, DOCX_LIMITS)?;
            // Parse a canonical package containing only the validated main
            // document. office_oxide otherwise eagerly follows relationship
            // fan-out into images, fonts, headers, and footers even though
            // plain-text extraction does not need those resources.
            drop(file_bytes);
            let document =
                DocxDocument::from_reader(Cursor::new(text_only_package)).map_err(|error| {
                    unreadable_word(format!(
                        "DOCX parser rejected the preflighted text-only package: {error}"
                    ))
                })?;
            document.plain_text()
        }
        WordFileType::Docx if is_cfb(&file_bytes) => {
            let inspection = inspect_cfb(&file_bytes)?;
            if inspection.is_encrypted_package {
                return Err(WORD_PASSWORD_ERROR.to_string());
            }
            return Err(WORD_FORMAT_MISMATCH_ERROR.to_string());
        }
        WordFileType::Docx => return Err(WORD_FORMAT_MISMATCH_ERROR.to_string()),
        WordFileType::Doc if is_cfb(&file_bytes) => {
            let text_only_container = preflight_legacy_doc(&file_bytes, max_extracted_text_bytes)?;
            drop(file_bytes);
            DocDocument::from_reader(Cursor::new(text_only_container))
                .map_err(|error| {
                    unreadable_word(format!(
                        "legacy DOC parser rejected the canonical text-only container: {error}"
                    ))
                })?
                .plain_text()
        }
        WordFileType::Doc => return Err(WORD_FORMAT_MISMATCH_ERROR.to_string()),
    };

    validate_extracted_text(text, max_extracted_text_bytes)
}

fn validate_extracted_text(text: String, max_bytes: usize) -> Result<String, String> {
    if text.len() > max_bytes {
        return Err(
            "The extracted text from this Word document is too large for Maple (max 10MB)."
                .to_string(),
        );
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        Err(WORD_EMPTY_ERROR.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn preflight_docx(file_bytes: &[u8], limits: DocxLimits) -> Result<Vec<u8>, String> {
    preflight_zip_directory(file_bytes, limits.max_entries)?;
    let mut archive = ZipArchive::new(Cursor::new(file_bytes))
        .map_err(|error| unreadable_word(format!("invalid DOCX ZIP container: {error}")))?;

    if archive.is_empty() {
        return Err(unreadable_word("DOCX ZIP container has no entries"));
    }
    if archive.len() > limits.max_entries {
        return Err(too_complex(format!(
            "DOCX ZIP contains {} entries (limit {})",
            archive.len(),
            limits.max_entries
        )));
    }

    let mut normalized_names = HashSet::with_capacity(archive.len());
    let mut total_metadata_bytes = 0_u64;
    let mut total_actual_bytes = 0_u64;
    let mut has_content_types = false;
    let mut has_package_relationships = false;
    let mut main_document_xml = None;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            unreadable_word(format!("could not inspect DOCX ZIP entry {index}: {error}"))
        })?;

        if entry.encrypted() {
            return Err(WORD_PASSWORD_ERROR.to_string());
        }
        if entry.is_symlink() {
            return Err(unreadable_word("DOCX ZIP contains a symbolic-link entry"));
        }

        let entry_name = entry
            .name()
            .map_err(|_| unreadable_word("DOCX ZIP contains an invalid part name encoding"))?
            .into_owned();
        let normalized_name = normalize_opc_entry_name(&entry_name)?;
        if !normalized_names.insert(normalized_name.clone()) {
            return Err(unreadable_word(format!(
                "DOCX ZIP contains an ambiguous duplicate part: {entry_name}"
            )));
        }

        if entry.is_dir() {
            continue;
        }

        has_content_types |= normalized_name == "[content_types].xml";
        has_package_relationships |= normalized_name == "_rels/.rels";

        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(unreadable_word(format!(
                "DOCX ZIP part uses unsupported compression: {entry_name}"
            )));
        }

        let declared_size = entry.size();
        if declared_size > limits.max_entry_bytes {
            return Err(too_complex(format!(
                "DOCX ZIP part {entry_name} expands to {declared_size} bytes"
            )));
        }
        total_metadata_bytes = total_metadata_bytes
            .checked_add(declared_size)
            .ok_or_else(|| too_complex("DOCX ZIP expanded-size total overflowed"))?;
        if total_metadata_bytes > limits.max_total_bytes {
            return Err(too_complex(format!(
                "DOCX ZIP expands to more than {} bytes",
                limits.max_total_bytes
            )));
        }

        let permitted_expansion = entry
            .compressed_size()
            .saturating_mul(limits.max_compression_ratio)
            .saturating_add(limits.compression_ratio_allowance);
        if declared_size > permitted_expansion {
            return Err(too_complex(format!(
                "DOCX ZIP part has an unsafe compression ratio: {entry_name}"
            )));
        }

        let is_xml = normalized_name.ends_with(".xml") || normalized_name.ends_with(".rels");
        let actual_size = if is_xml {
            let initial_capacity = usize::try_from(declared_size.min(1024 * 1024)).unwrap_or(0);
            let mut xml = Vec::with_capacity(initial_capacity);
            entry
                .by_ref()
                .take(limits.max_entry_bytes + 1)
                .read_to_end(&mut xml)
                .map_err(|error| {
                    unreadable_word(format!(
                        "could not decompress DOCX XML part {entry_name}: {error}"
                    ))
                })?;
            validate_xml_part(&normalized_name, &xml, limits)?;
            let actual_size = xml.len() as u64;
            if normalized_name == "word/document.xml" {
                main_document_xml = Some(xml);
            }
            actual_size
        } else {
            io::copy(
                &mut entry.by_ref().take(limits.max_entry_bytes + 1),
                &mut io::sink(),
            )
            .map_err(|error| {
                unreadable_word(format!(
                    "could not decompress DOCX part {entry_name}: {error}"
                ))
            })?
        };

        if actual_size != declared_size {
            return Err(unreadable_word(format!(
                "DOCX ZIP part size disagrees with its directory entry: {entry_name}"
            )));
        }
        total_actual_bytes = total_actual_bytes
            .checked_add(actual_size)
            .ok_or_else(|| too_complex("DOCX ZIP actual-size total overflowed"))?;
        if total_actual_bytes > limits.max_total_bytes {
            return Err(too_complex(format!(
                "DOCX ZIP actually expands to more than {} bytes",
                limits.max_total_bytes
            )));
        }
    }

    let Some(main_document_xml) = main_document_xml else {
        return Err(unreadable_word(
            "DOCX package is missing its main WordprocessingML document",
        ));
    };
    if !has_content_types || !has_package_relationships {
        return Err(unreadable_word(
            "DOCX package is missing a required WordprocessingML part",
        ));
    }

    build_text_only_docx(&main_document_xml)
}

fn build_text_only_docx(main_document_xml: &[u8]) -> Result<Vec<u8>, String> {
    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, contents) in [
        ("[Content_Types].xml", TEXT_ONLY_CONTENT_TYPES),
        ("_rels/.rels", TEXT_ONLY_PACKAGE_RELS),
        ("word/document.xml", main_document_xml),
    ] {
        writer.start_file(name, options).map_err(|error| {
            unreadable_word(format!("could not create safe DOCX package: {error}"))
        })?;
        writer.write_all(contents).map_err(|error| {
            unreadable_word(format!("could not write safe DOCX package: {error}"))
        })?;
    }
    writer
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|error| unreadable_word(format!("could not finish safe DOCX package: {error}")))
}

fn preflight_zip_directory(file_bytes: &[u8], max_entries: usize) -> Result<(), String> {
    const EOCD_LEN: usize = 22;
    const CENTRAL_HEADER_LEN: usize = 46;
    const MAX_ZIP_COMMENT_LEN: usize = u16::MAX as usize;

    if file_bytes.len() < EOCD_LEN {
        return Err(unreadable_word("DOCX ZIP is missing its end record"));
    }

    let search_start = file_bytes
        .len()
        .saturating_sub(EOCD_LEN + MAX_ZIP_COMMENT_LEN);
    let eocd_offset = (search_start..=file_bytes.len() - EOCD_LEN)
        .rev()
        .find(|&offset| {
            file_bytes.get(offset..offset + 4) == Some(b"PK\x05\x06")
                && read_le_u16(file_bytes, offset + 20)
                    .and_then(|length| offset.checked_add(EOCD_LEN + length as usize))
                    == Some(file_bytes.len())
        })
        .ok_or_else(|| unreadable_word("DOCX ZIP has no valid end record"))?;

    let disk_number = read_le_u16(file_bytes, eocd_offset + 4).unwrap_or(u16::MAX);
    let central_directory_disk = read_le_u16(file_bytes, eocd_offset + 6).unwrap_or(u16::MAX);
    let entries_on_disk = read_le_u16(file_bytes, eocd_offset + 8).unwrap_or(u16::MAX);
    let total_entries = read_le_u16(file_bytes, eocd_offset + 10).unwrap_or(u16::MAX);
    let central_directory_size =
        read_le_u32(file_bytes, eocd_offset + 12).unwrap_or(u32::MAX) as u64;
    let central_directory_offset =
        read_le_u32(file_bytes, eocd_offset + 16).unwrap_or(u32::MAX) as u64;

    if disk_number != 0 || central_directory_disk != 0 || entries_on_disk != total_entries {
        return Err(unreadable_word(
            "DOCX ZIP uses an unsupported multi-disk layout",
        ));
    }
    if total_entries == u16::MAX
        || central_directory_size == u32::MAX as u64
        || central_directory_offset == u32::MAX as u64
    {
        return Err(too_complex("DOCX ZIP64 packages are not supported"));
    }
    if total_entries as usize > max_entries {
        return Err(too_complex(format!(
            "DOCX ZIP declares {total_entries} entries (limit {max_entries})"
        )));
    }
    if central_directory_size > MAX_ZIP_CENTRAL_DIRECTORY_BYTES {
        return Err(too_complex("DOCX ZIP central directory is too large"));
    }

    let central_start = usize::try_from(central_directory_offset)
        .map_err(|_| unreadable_word("DOCX ZIP central-directory offset is invalid"))?;
    let central_size = usize::try_from(central_directory_size)
        .map_err(|_| unreadable_word("DOCX ZIP central-directory size is invalid"))?;
    let central_end = central_start
        .checked_add(central_size)
        .ok_or_else(|| unreadable_word("DOCX ZIP central-directory range overflowed"))?;
    if central_end != eocd_offset || central_end > file_bytes.len() {
        return Err(unreadable_word(
            "DOCX ZIP central-directory range is inconsistent",
        ));
    }

    let mut cursor = central_start;
    for _ in 0..total_entries {
        let fixed_end = cursor
            .checked_add(CENTRAL_HEADER_LEN)
            .ok_or_else(|| unreadable_word("DOCX ZIP central-directory entry overflowed"))?;
        if fixed_end > central_end || file_bytes.get(cursor..cursor + 4) != Some(b"PK\x01\x02") {
            return Err(unreadable_word(
                "DOCX ZIP central directory contains a malformed entry",
            ));
        }

        let general_purpose_flags = read_le_u16(file_bytes, cursor + 8).unwrap_or(u16::MAX);
        if general_purpose_flags & 0x0001 != 0 {
            return Err(WORD_PASSWORD_ERROR.to_string());
        }
        let compressed_size = read_le_u32(file_bytes, cursor + 20).unwrap_or(u32::MAX);
        let uncompressed_size = read_le_u32(file_bytes, cursor + 24).unwrap_or(u32::MAX);
        let name_len = read_le_u16(file_bytes, cursor + 28).unwrap_or(u16::MAX) as usize;
        let extra_len = read_le_u16(file_bytes, cursor + 30).unwrap_or(u16::MAX) as usize;
        let comment_len = read_le_u16(file_bytes, cursor + 32).unwrap_or(u16::MAX) as usize;
        let start_disk = read_le_u16(file_bytes, cursor + 34).unwrap_or(u16::MAX);
        let local_header_offset = read_le_u32(file_bytes, cursor + 42).unwrap_or(u32::MAX);
        if compressed_size == u32::MAX
            || uncompressed_size == u32::MAX
            || local_header_offset == u32::MAX
            || start_disk != 0
        {
            return Err(too_complex("DOCX ZIP64 entry metadata is not supported"));
        }

        cursor = fixed_end
            .checked_add(name_len)
            .and_then(|next| next.checked_add(extra_len))
            .and_then(|next| next.checked_add(comment_len))
            .ok_or_else(|| unreadable_word("DOCX ZIP central-directory entry overflowed"))?;
        if cursor > central_end {
            return Err(unreadable_word(
                "DOCX ZIP central-directory entry is truncated",
            ));
        }
    }
    if cursor != central_end {
        return Err(unreadable_word(
            "DOCX ZIP central-directory entry count is inconsistent",
        ));
    }

    Ok(())
}

fn validate_xml_part(name: &str, raw_xml: &[u8], limits: DocxLimits) -> Result<(), String> {
    if std::str::from_utf8(raw_xml).is_err() {
        return Err(unreadable_word(format!(
            "DOCX XML part is not valid UTF-8 text: {name}"
        )));
    }
    let xml = raw_xml;

    let mut reader = Reader::from_reader(xml);
    let config = reader.config_mut();
    config.check_end_names = true;
    config.check_comments = true;

    let mut depth = 0_usize;
    let mut event_count = 0_usize;
    let mut total_attributes = 0_usize;
    let mut root_count = 0_usize;
    let mut model_elements = 0_usize;
    let mut table_depth = 0_usize;

    loop {
        event_count = event_count
            .checked_add(1)
            .ok_or_else(|| too_complex(format!("XML event count overflowed in {name}")))?;
        if event_count > limits.max_xml_events {
            return Err(too_complex(format!(
                "DOCX XML part has too many nodes: {name}"
            )));
        }

        match reader.read_event() {
            Ok(Event::Start(start)) => {
                if depth == 0 {
                    root_count += 1;
                }
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| too_complex(format!("XML depth overflowed in {name}")))?;
                if depth > limits.max_xml_depth {
                    return Err(too_complex(format!(
                        "DOCX XML part is nested too deeply: {name}"
                    )));
                }
                validate_xml_attributes(name, &start, &mut total_attributes, limits)?;
                validate_docx_model_element(
                    name,
                    &start,
                    &mut model_elements,
                    &mut table_depth,
                    limits,
                )?;
            }
            Ok(Event::Empty(start)) => {
                if depth == 0 {
                    root_count += 1;
                }
                validate_xml_attributes(name, &start, &mut total_attributes, limits)?;
                let mut empty_table_depth = table_depth;
                validate_docx_model_element(
                    name,
                    &start,
                    &mut model_elements,
                    &mut empty_table_depth,
                    limits,
                )?;
            }
            Ok(Event::End(end)) => {
                if name == "word/document.xml" && end.local_name().as_ref() == b"tbl" {
                    table_depth = table_depth
                        .checked_sub(1)
                        .ok_or_else(|| unreadable_word("unbalanced DOCX table structure"))?;
                }
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| unreadable_word(format!("unbalanced XML in {name}")))?;
            }
            Ok(Event::DocType(_)) => {
                return Err(unreadable_word(format!(
                    "DOCX XML part contains a forbidden document-type declaration: {name}"
                )));
            }
            Ok(Event::Decl(declaration)) => {
                if let Some(encoding) = declaration.encoding() {
                    let encoding = encoding.map_err(|error| {
                        unreadable_word(format!(
                            "DOCX XML part has a malformed encoding declaration in {name}: {error}"
                        ))
                    })?;
                    if !encoding.eq_ignore_ascii_case(b"utf-8") {
                        return Err(unreadable_word(format!(
                            "DOCX XML part uses a non-UTF-8 encoding: {name}"
                        )));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(unreadable_word(format!(
                    "malformed DOCX XML part {name}: {error}"
                )));
            }
        }
    }

    if depth != 0 || root_count != 1 {
        return Err(unreadable_word(format!(
            "DOCX XML part does not contain one balanced root element: {name}"
        )));
    }

    Ok(())
}

fn validate_docx_model_element(
    part_name: &str,
    start: &BytesStart<'_>,
    model_elements: &mut usize,
    table_depth: &mut usize,
    limits: DocxLimits,
) -> Result<(), String> {
    if part_name != "word/document.xml" {
        return Ok(());
    }

    let local_name = start.local_name();
    let local_name = local_name.as_ref();
    if matches!(
        local_name,
        b"p" | b"r"
            | b"t"
            | b"br"
            | b"tab"
            | b"drawing"
            | b"tbl"
            | b"tr"
            | b"tc"
            | b"hyperlink"
            | b"sdt"
    ) {
        *model_elements = model_elements
            .checked_add(1)
            .ok_or_else(|| too_complex("DOCX model-element count overflowed"))?;
        if *model_elements > limits.max_model_elements {
            return Err(too_complex(
                "DOCX main document contains too many structural elements",
            ));
        }
    }

    if local_name == b"tbl" {
        *table_depth = table_depth
            .checked_add(1)
            .ok_or_else(|| too_complex("DOCX table nesting depth overflowed"))?;
        if *table_depth > limits.max_table_depth {
            return Err(too_complex("DOCX tables are nested too deeply"));
        }
    }

    Ok(())
}

fn validate_xml_attributes(
    part_name: &str,
    start: &BytesStart<'_>,
    total_attributes: &mut usize,
    limits: DocxLimits,
) -> Result<(), String> {
    let mut element_attributes = 0_usize;
    for attribute in start.attributes() {
        attribute.map_err(|error| {
            unreadable_word(format!(
                "malformed XML attribute in DOCX part {part_name}: {error}"
            ))
        })?;
        element_attributes += 1;
        *total_attributes = total_attributes
            .checked_add(1)
            .ok_or_else(|| too_complex(format!("XML attribute count overflowed in {part_name}")))?;
        if element_attributes > limits.max_attributes_per_element
            || *total_attributes > limits.max_total_attributes
        {
            return Err(too_complex(format!(
                "DOCX XML part has too many attributes: {part_name}"
            )));
        }
    }
    Ok(())
}

fn normalize_opc_entry_name(name: &str) -> Result<String, String> {
    if name.is_empty() || name.len() > MAX_OPC_PATH_BYTES || name.contains('\0') {
        return Err(unreadable_word("DOCX ZIP contains an invalid part name"));
    }

    let slash_normalized = name.replace('\\', "/");
    if slash_normalized.starts_with('/')
        || slash_normalized.contains('?')
        || slash_normalized.contains('#')
    {
        return Err(unreadable_word(format!(
            "DOCX ZIP contains an invalid part name: {name}"
        )));
    }

    let decoded = decode_percent_encoding(&slash_normalized);
    let trimmed = decoded.strip_suffix('/').unwrap_or(&decoded);
    if trimmed.is_empty()
        || trimmed.split('/').any(|segment| {
            segment.is_empty() || segment == "." || segment == ".." || segment.ends_with('.')
        })
    {
        return Err(unreadable_word(format!(
            "DOCX ZIP contains an invalid part path: {name}"
        )));
    }

    Ok(trimmed.to_ascii_lowercase())
}

fn decode_percent_encoding(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(decoded).unwrap_or_else(|_| value.to_string())
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

struct CfbInspection {
    is_encrypted_package: bool,
}

fn inspect_cfb(file_bytes: &[u8]) -> Result<CfbInspection, String> {
    preflight_cfb_header_and_difat(file_bytes)?;
    let compound = CompoundFile::open(Cursor::new(file_bytes))
        .map_err(|error| unreadable_word(format!("invalid CFB container: {error}")))?;

    let mut total_stream_bytes = 0_u64;
    for path in [
        "/WordDocument",
        "/0Table",
        "/1Table",
        "/Data",
        "/EncryptionInfo",
        "/EncryptedPackage",
    ] {
        let Ok(entry) = compound.entry(path) else {
            continue;
        };
        if !entry.is_stream() {
            return Err(unreadable_word(format!(
                "CFB object {path} is not a stream"
            )));
        }
        if entry.len() > MAX_CFB_STREAM_BYTES {
            return Err(too_complex(format!("CFB stream {path} is too large")));
        }
        total_stream_bytes = total_stream_bytes
            .checked_add(entry.len())
            .ok_or_else(|| too_complex("CFB stream-size total overflowed"))?;
        if total_stream_bytes > MAX_CFB_TOTAL_STREAM_BYTES {
            return Err(too_complex("CFB streams exceed Maple's safe total"));
        }
    }

    Ok(CfbInspection {
        is_encrypted_package: compound.is_stream("/EncryptionInfo")
            || compound.is_stream("/EncryptedPackage"),
    })
}

fn preflight_cfb_header_and_difat(file_bytes: &[u8]) -> Result<(), String> {
    const CFB_HEADER_LEN: usize = 512;
    const HEADER_DIFAT_ENTRIES: usize = 109;

    if file_bytes.len() < CFB_HEADER_LEN || !is_cfb(file_bytes) {
        return Err(unreadable_word("invalid CFB header"));
    }
    let major_version =
        read_le_u16(file_bytes, 0x1a).ok_or_else(|| unreadable_word("truncated CFB version"))?;
    let byte_order =
        read_le_u16(file_bytes, 0x1c).ok_or_else(|| unreadable_word("truncated CFB byte order"))?;
    let sector_shift = read_le_u16(file_bytes, 0x1e)
        .ok_or_else(|| unreadable_word("truncated CFB sector size"))?;
    let mini_sector_shift = read_le_u16(file_bytes, 0x20)
        .ok_or_else(|| unreadable_word("truncated CFB mini-sector size"))?;
    let expected_sector_shift = match major_version {
        3 => 9,
        4 => 12,
        _ => return Err(unreadable_word("unsupported CFB major version")),
    };
    if byte_order != 0xfffe
        || sector_shift != expected_sector_shift
        || mini_sector_shift != 6
        || read_le_u32(file_bytes, 0x38) != Some(4_096)
    {
        return Err(unreadable_word("invalid CFB storage geometry"));
    }

    let sector_size = 1_usize << sector_shift;
    if file_bytes.len() < sector_size || !file_bytes.len().is_multiple_of(sector_size) {
        return Err(unreadable_word("CFB file is not sector-aligned"));
    }
    let sector_count = file_bytes.len() / sector_size - 1;
    if sector_count == 0 || sector_count > CFB_MAX_REGULAR_SECTOR as usize {
        return Err(unreadable_word("CFB physical sector count is invalid"));
    }

    let directory_sector_count = read_le_u32(file_bytes, 0x28).unwrap_or(u32::MAX) as usize;
    let fat_sector_count = read_le_u32(file_bytes, 0x2c).unwrap_or(u32::MAX) as usize;
    let minifat_sector_count = read_le_u32(file_bytes, 0x40).unwrap_or(u32::MAX) as usize;
    let first_difat_sector = read_le_u32(file_bytes, 0x44).unwrap_or(CFB_FREE_SECTOR);
    let difat_sector_count = read_le_u32(file_bytes, 0x48).unwrap_or(u32::MAX) as usize;
    if (major_version == 3 && directory_sector_count != 0)
        || directory_sector_count > sector_count
        || fat_sector_count > sector_count
        || minifat_sector_count > sector_count
        || difat_sector_count > sector_count
    {
        return Err(too_complex("CFB header declares too many sectors"));
    }

    let fat_entries_per_sector = sector_size / 4;
    let minimum_fat_sectors = sector_count.div_ceil(fat_entries_per_sector);
    if fat_sector_count
        .checked_mul(fat_entries_per_sector)
        .is_none_or(|capacity| capacity < sector_count)
        || fat_sector_count > minimum_fat_sectors.saturating_add(1)
    {
        return Err(unreadable_word("CFB FAT sector count is inconsistent"));
    }

    let mut fat_sector_ids = Vec::with_capacity(fat_sector_count);
    let mut seen_fat_sectors = HashSet::with_capacity(fat_sector_count);
    for index in 0..HEADER_DIFAT_ENTRIES {
        let sector_id = read_le_u32(file_bytes, 0x4c + index * 4).unwrap_or(CFB_FREE_SECTOR);
        collect_fat_sector_id(
            sector_id,
            sector_count,
            fat_sector_count,
            &mut fat_sector_ids,
            &mut seen_fat_sectors,
        )?;
    }

    let mut current_difat_sector = first_difat_sector;
    let mut seen_difat_sectors = HashSet::with_capacity(difat_sector_count);
    for index in 0..difat_sector_count {
        let sector_id = regular_cfb_sector_id(
            current_difat_sector,
            sector_count,
            "CFB DIFAT chain contains an invalid sector",
        )?;
        if !seen_difat_sectors.insert(sector_id) || seen_fat_sectors.contains(&sector_id) {
            return Err(unreadable_word(
                "CFB DIFAT and FAT sector declarations overlap or repeat",
            ));
        }
        let sector = cfb_sector(file_bytes, sector_size, sector_id)?;
        for entry in 0..fat_entries_per_sector - 1 {
            let fat_sector = read_le_u32(sector, entry * 4).unwrap_or(CFB_FREE_SECTOR);
            collect_fat_sector_id(
                fat_sector,
                sector_count,
                fat_sector_count,
                &mut fat_sector_ids,
                &mut seen_fat_sectors,
            )?;
        }

        let next = read_le_u32(sector, sector_size - 4).unwrap_or(CFB_FREE_SECTOR);
        if index + 1 == difat_sector_count {
            if next != CFB_END_OF_CHAIN {
                return Err(unreadable_word("CFB DIFAT chain has no valid terminator"));
            }
        } else {
            current_difat_sector = regular_cfb_sector_id(
                next,
                sector_count,
                "CFB DIFAT chain ends before its declared length",
            )?;
        }
    }
    if difat_sector_count == 0 && !matches!(first_difat_sector, CFB_END_OF_CHAIN | CFB_FREE_SECTOR)
    {
        return Err(unreadable_word(
            "CFB header declares an unexpected DIFAT chain",
        ));
    }
    if fat_sector_ids.len() != fat_sector_count {
        return Err(unreadable_word(
            "CFB FAT sector count does not match its DIFAT",
        ));
    }
    if seen_difat_sectors
        .iter()
        .any(|sector_id| seen_fat_sectors.contains(sector_id))
    {
        return Err(unreadable_word("CFB FAT and DIFAT sectors overlap"));
    }

    Ok(())
}

fn collect_fat_sector_id(
    sector_id: u32,
    sector_count: usize,
    declared_count: usize,
    collected: &mut Vec<u32>,
    seen: &mut HashSet<u32>,
) -> Result<(), String> {
    if collected.len() == declared_count {
        if sector_id != CFB_FREE_SECTOR {
            return Err(unreadable_word("CFB DIFAT contains non-free padding"));
        }
        return Ok(());
    }

    let sector_id = regular_cfb_sector_id(
        sector_id,
        sector_count,
        "CFB DIFAT contains an invalid FAT sector",
    )?;
    if !seen.insert(sector_id) {
        return Err(unreadable_word("CFB DIFAT repeats a FAT sector"));
    }
    collected.push(sector_id);
    Ok(())
}

fn regular_cfb_sector_id(
    sector_id: u32,
    sector_count: usize,
    reason: &'static str,
) -> Result<u32, String> {
    if sector_id > CFB_MAX_REGULAR_SECTOR || sector_id as usize >= sector_count {
        Err(unreadable_word(reason))
    } else {
        Ok(sector_id)
    }
}

fn cfb_sector(file_bytes: &[u8], sector_size: usize, sector_id: u32) -> Result<&[u8], String> {
    let offset = (sector_id as usize)
        .checked_add(1)
        .and_then(|index| index.checked_mul(sector_size))
        .ok_or_else(|| unreadable_word("CFB sector offset overflowed"))?;
    let end = offset
        .checked_add(sector_size)
        .ok_or_else(|| unreadable_word("CFB sector range overflowed"))?;
    file_bytes
        .get(offset..end)
        .ok_or_else(|| unreadable_word("CFB sector lies outside the file"))
}

fn preflight_legacy_doc(
    file_bytes: &[u8],
    max_extracted_text_bytes: usize,
) -> Result<Vec<u8>, String> {
    let inspection = inspect_cfb(file_bytes)?;
    if inspection.is_encrypted_package {
        return Err(WORD_PASSWORD_ERROR.to_string());
    }

    let mut compound = CompoundFile::open(Cursor::new(file_bytes))
        .map_err(|error| unreadable_word(format!("could not reopen CFB container: {error}")))?;
    if !compound.is_stream("/WordDocument") {
        return Err(WORD_FORMAT_MISMATCH_ERROR.to_string());
    }

    let word_document = read_cfb_stream(&mut compound, "/WordDocument")?;
    if read_le_u16(&word_document, 0) != Some(0xa5ec) {
        return Err(unreadable_word(
            "legacy DOC does not use the supported Word 97-2003 FIB",
        ));
    }
    let flags = read_le_u16(&word_document, 0x0a)
        .ok_or_else(|| unreadable_word("legacy DOC FIB is truncated"))?;
    if flags & 0x8100 != 0 {
        return Err(WORD_PASSWORD_ERROR.to_string());
    }

    let preferred_table = if flags & 0x0200 != 0 {
        "/1Table"
    } else {
        "/0Table"
    };
    let fallback_table = if preferred_table == "/1Table" {
        "/0Table"
    } else {
        "/1Table"
    };
    let table_path = if compound.is_stream(preferred_table) {
        preferred_table
    } else if compound.is_stream(fallback_table) {
        fallback_table
    } else {
        return Err(unreadable_word(
            "legacy DOC is missing its 0Table/1Table stream",
        ));
    };
    let table = read_cfb_stream(&mut compound, table_path)?;
    validate_legacy_clx(&word_document, &table, max_extracted_text_bytes)?;

    build_text_only_cfb(&word_document, table_path, &table)
}

fn read_cfb_stream(
    compound: &mut CompoundFile<Cursor<&[u8]>>,
    path: &str,
) -> Result<Vec<u8>, String> {
    let entry = compound.entry(path).map_err(|error| {
        unreadable_word(format!("could not inspect CFB stream {path}: {error}"))
    })?;
    if !entry.is_stream() || entry.len() > MAX_CFB_STREAM_BYTES {
        return Err(too_complex(format!(
            "CFB stream {path} is invalid or too large"
        )));
    }
    let declared_size = entry.len();
    let mut stream = compound
        .open_stream(path)
        .map_err(|error| unreadable_word(format!("could not open CFB stream {path}: {error}")))?;
    let mut contents = Vec::with_capacity(usize::try_from(declared_size).unwrap_or(0));
    stream
        .by_ref()
        .take(MAX_CFB_STREAM_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| unreadable_word(format!("could not read CFB stream {path}: {error}")))?;
    if contents.len() as u64 != declared_size {
        return Err(unreadable_word(format!(
            "CFB stream {path} size disagrees with its directory entry"
        )));
    }
    Ok(contents)
}

fn validate_legacy_clx(
    word_document: &[u8],
    table: &[u8],
    max_extracted_text_bytes: usize,
) -> Result<(), String> {
    const FIB_CLX_OFFSET: usize = 0x01a2;
    const FIB_CLX_SIZE: usize = 0x01a6;
    const FIB_MAIN_TEXT_LENGTH: usize = 0x4c;

    let main_text_chars = read_le_u32(word_document, FIB_MAIN_TEXT_LENGTH)
        .ok_or_else(|| unreadable_word("legacy DOC FIB has no main-text length"))?
        as usize;
    let clx_offset = read_le_u32(word_document, FIB_CLX_OFFSET)
        .ok_or_else(|| unreadable_word("legacy DOC FIB has no CLX offset"))?
        as usize;
    let clx_size = read_le_u32(word_document, FIB_CLX_SIZE)
        .ok_or_else(|| unreadable_word("legacy DOC FIB has no CLX size"))?
        as usize;
    let clx_end = clx_offset
        .checked_add(clx_size)
        .ok_or_else(|| unreadable_word("legacy DOC CLX range overflowed"))?;
    let clx = table
        .get(clx_offset..clx_end)
        .ok_or_else(|| unreadable_word("legacy DOC CLX lies outside its table stream"))?;

    let mut cursor = 0_usize;
    while clx.get(cursor) == Some(&0x01) {
        let grpprl_size = read_le_u16(clx, cursor + 1)
            .ok_or_else(|| unreadable_word("legacy DOC CLX grpprl is truncated"))?
            as usize;
        cursor = cursor
            .checked_add(3)
            .and_then(|offset| offset.checked_add(grpprl_size))
            .filter(|&offset| offset <= clx.len())
            .ok_or_else(|| unreadable_word("legacy DOC CLX grpprl range is invalid"))?;
    }
    if clx.get(cursor) != Some(&0x02) {
        return Err(unreadable_word(
            "legacy DOC CLX does not contain a piece table",
        ));
    }
    let plc_size = read_le_u32(clx, cursor + 1)
        .ok_or_else(|| unreadable_word("legacy DOC piece-table size is truncated"))?
        as usize;
    let plc_start = cursor
        .checked_add(5)
        .ok_or_else(|| unreadable_word("legacy DOC piece-table offset overflowed"))?;
    let plc_end = plc_start
        .checked_add(plc_size)
        .ok_or_else(|| unreadable_word("legacy DOC piece-table range overflowed"))?;
    if plc_end != clx.len() || plc_size < 4 || !(plc_size - 4).is_multiple_of(12) {
        return Err(unreadable_word(
            "legacy DOC piece table has an invalid layout",
        ));
    }
    let plc = &clx[plc_start..plc_end];
    let piece_count = (plc_size - 4) / 12;
    if piece_count == 0 || piece_count > MAX_LEGACY_DOC_PIECES {
        return Err(too_complex(format!(
            "legacy DOC contains {piece_count} text pieces"
        )));
    }

    let cp_array_bytes = (piece_count + 1)
        .checked_mul(4)
        .ok_or_else(|| too_complex("legacy DOC CP array size overflowed"))?;
    if read_le_u32(plc, 0) != Some(0) || cp_array_bytes > plc.len() {
        return Err(unreadable_word(
            "legacy DOC piece table has an invalid initial character position",
        ));
    }

    let mut predicted_output_bytes = 0_usize;
    let mut source_work_bytes = 0_usize;
    let mut previous_cp = 0_usize;
    for index in 0..piece_count {
        let cp_start = read_le_u32(plc, index * 4).unwrap_or(u32::MAX) as usize;
        let cp_end = read_le_u32(plc, (index + 1) * 4).unwrap_or(u32::MAX) as usize;
        if cp_start != previous_cp || cp_end < cp_start {
            return Err(unreadable_word(
                "legacy DOC character positions are not monotonic",
            ));
        }
        previous_cp = cp_end;
        if cp_start >= main_text_chars {
            break;
        }

        let char_count = cp_end.min(main_text_chars) - cp_start;
        let pcd_offset = cp_array_bytes
            .checked_add(index * 8)
            .ok_or_else(|| unreadable_word("legacy DOC PCD offset overflowed"))?;
        let encoded_fc = read_le_u32(plc, pcd_offset + 2)
            .ok_or_else(|| unreadable_word("legacy DOC PCD is truncated"))?;
        let compressed = encoded_fc & 0x4000_0000 != 0;
        let byte_offset = if compressed {
            ((encoded_fc & !0x4000_0000) / 2) as usize
        } else {
            encoded_fc as usize
        };
        let byte_count = if compressed {
            char_count
        } else {
            char_count
                .checked_mul(2)
                .ok_or_else(|| too_complex("legacy DOC Unicode piece size overflowed"))?
        };
        let byte_end = byte_offset
            .checked_add(byte_count)
            .ok_or_else(|| unreadable_word("legacy DOC text-piece range overflowed"))?;
        let source = word_document
            .get(byte_offset..byte_end)
            .ok_or_else(|| unreadable_word("legacy DOC text piece lies outside WordDocument"))?;

        source_work_bytes = source_work_bytes
            .checked_add(source.len())
            .ok_or_else(|| too_complex("legacy DOC source-work budget overflowed"))?;
        if source_work_bytes > word_document.len() {
            return Err(too_complex(
                "legacy DOC text pieces repeat too much source data",
            ));
        }
        let piece_output_bytes = if compressed {
            source
                .iter()
                .map(|&byte| legacy_cp1252_utf8_len(byte))
                .sum()
        } else {
            char::decode_utf16(
                source
                    .chunks_exact(2)
                    .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]])),
            )
            .map(|decoded| decoded.unwrap_or(char::REPLACEMENT_CHARACTER).len_utf8())
            .sum()
        };
        predicted_output_bytes = predicted_output_bytes
            .checked_add(piece_output_bytes)
            .ok_or_else(|| too_complex("legacy DOC output-size budget overflowed"))?;
        if predicted_output_bytes > max_extracted_text_bytes {
            return Err(too_complex(
                "legacy DOC decoded text exceeds Maple's output budget",
            ));
        }
    }

    Ok(())
}

fn legacy_cp1252_utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0x80 | 0x82..=0x89 | 0x8b | 0x91..=0x99 | 0x9b => 3,
        _ => 2,
    }
}

fn build_text_only_cfb(
    word_document: &[u8],
    table_path: &str,
    table: &[u8],
) -> Result<Vec<u8>, String> {
    let cursor = Cursor::new(Vec::new());
    let mut compound = CompoundFile::create(cursor).map_err(|error| {
        unreadable_word(format!("could not create safe DOC container: {error}"))
    })?;
    for (path, contents) in [("/WordDocument", word_document), (table_path, table)] {
        let mut stream = compound.create_stream(path).map_err(|error| {
            unreadable_word(format!("could not create safe DOC stream {path}: {error}"))
        })?;
        stream.write_all(contents).map_err(|error| {
            unreadable_word(format!("could not write safe DOC stream {path}: {error}"))
        })?;
    }
    Ok(compound.into_inner().into_inner())
}

fn read_le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let bytes = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let bytes = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn is_zip(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some(b"PK\x03\x04") | Some(b"PK\x05\x06") | Some(b"PK\x07\x08")
    )
}

fn is_cfb(bytes: &[u8]) -> bool {
    bytes.starts_with(CFB_MAGIC)
}

fn unreadable_word(reason: impl std::fmt::Display) -> String {
    log::warn!("Word document validation failed: {reason}");
    WORD_READ_ERROR.to_string()
}

fn too_complex(reason: impl std::fmt::Display) -> String {
    log::warn!("Word document exceeded a safe processing limit: {reason}");
    WORD_COMPLEXITY_ERROR.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        extract_word_document, normalize_opc_entry_name, preflight_cfb_header_and_difat,
        preflight_docx, preflight_legacy_doc, validate_extracted_text, validate_legacy_clx,
        DocxLimits, WordFileType, CFB_END_OF_CHAIN, DOCX_LIMITS, MAX_LEGACY_DOC_PIECES,
        WORD_COMPLEXITY_ERROR, WORD_EMPTY_ERROR, WORD_FORMAT_MISMATCH_ERROR, WORD_PASSWORD_ERROR,
        WORD_READ_ERROR,
    };
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
    use std::io::{Cursor, Read, Seek, SeekFrom, Write};
    use zip::write::{SimpleFileOptions, ZipWriter};
    use zip::{CompressionMethod, ZipArchive};

    #[test]
    fn extracts_docx_paragraphs_unicode_tabs_and_tables() {
        let body = r#"
          <w:p><w:r><w:t>Hello, </w:t><w:t>Maple 🍁</w:t><w:tab/><w:t>DOCX</w:t></w:r></w:p>
          <w:tbl><w:tr>
            <w:tc><w:p><w:r><w:t>Left cell</w:t></w:r></w:p></w:tc>
            <w:tc><w:p><w:r><w:t>Right cell</w:t></w:r></w:p></w:tc>
          </w:tr></w:tbl>
        "#;

        let text = extract_word_document(minimal_docx(body), WordFileType::Docx, 10 * 1024 * 1024)
            .expect("valid DOCX should extract");

        assert!(text.contains("Hello, Maple 🍁\tDOCX"), "{text:?}");
        assert!(text.contains("Left cell\tRight cell"), "{text:?}");
    }

    #[test]
    fn extracts_a_real_legacy_doc_fixture() {
        let text = extract_word_document(legacy_doc_fixture(), WordFileType::Doc, 10 * 1024 * 1024)
            .expect("valid legacy DOC should extract");

        assert!(text.contains("Outer cell text"), "{text:?}");
        assert!(text.contains("Inner cell text"), "{text:?}");
    }

    #[test]
    fn rejects_password_protected_legacy_doc() {
        let cursor = Cursor::new(legacy_doc_fixture());
        let mut compound = cfb::CompoundFile::open(cursor).expect("open fixture CFB");
        {
            let mut stream = compound
                .open_stream("/WordDocument")
                .expect("open WordDocument stream");
            stream.seek(SeekFrom::Start(10)).expect("seek to FIB flags");
            let mut bytes = [0_u8; 2];
            stream.read_exact(&mut bytes).expect("read FIB flags");
            let encrypted_flags = u16::from_le_bytes(bytes) | 0x0100;
            stream.seek(SeekFrom::Start(10)).expect("rewind to flags");
            stream
                .write_all(&encrypted_flags.to_le_bytes())
                .expect("write encrypted flag");
        }
        let encrypted = compound.into_inner().into_inner();

        let error = extract_word_document(encrypted, WordFileType::Doc, 10 * 1024 * 1024)
            .expect_err("encrypted DOC should fail");
        assert_eq!(error, WORD_PASSWORD_ERROR);
    }

    #[test]
    fn rejects_a_self_referential_difat_without_looping() {
        let fixture = legacy_doc_fixture();
        let mut malformed = fixture[..512].to_vec();
        malformed.resize(1024, 0xff);
        // Route the external DIFAT to sector 0, then point that sector back to
        // itself. The cycle-aware CFB preflight must return a normal error.
        malformed[68..72].copy_from_slice(&0_u32.to_le_bytes());
        malformed[72..76].copy_from_slice(&1_u32.to_le_bytes());
        malformed[76..512].fill(0xff);
        malformed[1020..1024].copy_from_slice(&0_u32.to_le_bytes());

        let error = preflight_legacy_doc(&malformed, 10 * 1024 * 1024)
            .expect_err("DIFAT cycle should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_an_external_difat_self_cycle_before_cfb_parsing() {
        const SECTOR_SIZE: usize = 512;
        const SECTOR_COUNT: usize = 14_000;
        const FAT_SECTOR_COUNT: usize = 110;
        const EXTERNAL_DIFAT_SECTOR: usize = 110;

        let mut malformed = vec![0_u8; (SECTOR_COUNT + 1) * SECTOR_SIZE];
        malformed[..8].copy_from_slice(super::CFB_MAGIC);
        malformed[0x1a..0x1c].copy_from_slice(&3_u16.to_le_bytes());
        malformed[0x1c..0x1e].copy_from_slice(&0xfffe_u16.to_le_bytes());
        malformed[0x1e..0x20].copy_from_slice(&9_u16.to_le_bytes());
        malformed[0x20..0x22].copy_from_slice(&6_u16.to_le_bytes());
        malformed[0x2c..0x30].copy_from_slice(&(FAT_SECTOR_COUNT as u32).to_le_bytes());
        malformed[0x30..0x34].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        malformed[0x38..0x3c].copy_from_slice(&4_096_u32.to_le_bytes());
        malformed[0x3c..0x40].copy_from_slice(&CFB_END_OF_CHAIN.to_le_bytes());
        malformed[0x44..0x48].copy_from_slice(&(EXTERNAL_DIFAT_SECTOR as u32).to_le_bytes());
        malformed[0x48..0x4c].copy_from_slice(&1_u32.to_le_bytes());
        for index in 0..109 {
            malformed[0x4c + index * 4..0x50 + index * 4]
                .copy_from_slice(&(index as u32).to_le_bytes());
        }

        let external_offset = (EXTERNAL_DIFAT_SECTOR + 1) * SECTOR_SIZE;
        malformed[external_offset..external_offset + SECTOR_SIZE].fill(0xff);
        malformed[external_offset..external_offset + 4].copy_from_slice(&109_u32.to_le_bytes());
        malformed[external_offset + SECTOR_SIZE - 4..external_offset + SECTOR_SIZE]
            .copy_from_slice(&(EXTERNAL_DIFAT_SECTOR as u32).to_le_bytes());

        let error = preflight_cfb_header_and_difat(&malformed)
            .expect_err("external DIFAT self-cycle should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_blank_docx() {
        let error = extract_word_document(
            minimal_docx("<w:p><w:r><w:t>   </w:t></w:r></w:p>"),
            WordFileType::Docx,
            10 * 1024 * 1024,
        )
        .expect_err("blank DOCX should fail");
        assert_eq!(error, WORD_EMPTY_ERROR);
    }

    #[test]
    fn rejects_a_file_whose_container_does_not_match_its_type() {
        let error = extract_word_document(
            minimal_docx("<w:p><w:r><w:t>hello</w:t></w:r></w:p>"),
            WordFileType::Doc,
            10 * 1024 * 1024,
        )
        .expect_err("DOC should not accept a DOCX container");
        assert_eq!(error, WORD_FORMAT_MISMATCH_ERROR);
    }

    #[test]
    fn rejects_ambiguous_case_folded_docx_parts() {
        let bytes = zip_entries(&[
            ("[Content_Types].xml", content_types_xml()),
            ("_rels/.rels", package_rels_xml()),
            ("word/document.xml", document_xml("<w:p/>")),
            ("WORD/document.xml", document_xml("<w:p/>")),
        ]);
        let error = preflight_docx(&bytes, DOCX_LIMITS).expect_err("duplicate should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_a_docx_main_document_symlink() {
        let bytes = docx_with_symlink("word/document.xml", "../../outside.xml");
        let error = extract_word_document(bytes, WordFileType::Docx, 10 * 1024 * 1024)
            .expect_err("a symbolic-link main document should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_a_docx_ancillary_symlink() {
        let bytes = docx_with_symlink("word/media/link", "../../../outside");
        let error = extract_word_document(bytes, WordFileType::Docx, 10 * 1024 * 1024)
            .expect_err("an unused symbolic-link part should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_docx_parts_with_unsafe_paths() {
        for name in [
            "../outside.xml",
            "word/../../outside.xml",
            r"word\..\..\outside.xml",
            "%2e%2e/outside.xml",
            "word/%2e%2e/%2e%2e/outside.xml",
            "/outside.xml",
        ] {
            let bytes = zip_entries(&[
                ("[Content_Types].xml", content_types_xml()),
                ("_rels/.rels", package_rels_xml()),
                (
                    "word/document.xml",
                    document_xml("<w:p><w:r><w:t>Safe text</w:t></w:r></w:p>"),
                ),
                (name, "<unused/>".to_string()),
            ]);
            let error = extract_word_document(bytes, WordFileType::Docx, 10 * 1024 * 1024)
                .expect_err("an unsafe ancillary part path should fail");
            assert_eq!(error, WORD_READ_ERROR, "part name: {name}");
        }
    }

    #[test]
    fn canonical_docx_discards_privileged_modes_and_unused_parts() {
        let mut bytes = zip_entries(&[
            ("[Content_Types].xml", content_types_xml()),
            ("_rels/.rels", package_rels_xml()),
            (
                "word/document.xml",
                document_xml("<w:p><w:r><w:t>Preserved Maple text</w:t></w:r></w:p>"),
            ),
            ("word/media/unused.bin", "unused".to_string()),
        ]);
        // ZipWriter intentionally masks these bits, so change the directory
        // metadata to model an archive produced by an untrusted application.
        set_zip_entry_mode(&mut bytes, "word/document.xml", 0o106755);
        let mut original = ZipArchive::new(Cursor::new(&bytes)).expect("open original package");
        assert_eq!(
            original
                .by_name("word/document.xml")
                .expect("original main document")
                .external_attributes()
                >> 16
                & 0o6000,
            0o6000,
        );

        let canonical = preflight_docx(&bytes, DOCX_LIMITS).expect("canonicalize valid package");
        let mut archive = ZipArchive::new(Cursor::new(&canonical)).expect("open canonical package");
        assert_eq!(archive.len(), 3);
        assert!(archive.by_name("word/media/unused.bin").is_err());
        for index in 0..archive.len() {
            let entry = archive.by_index(index).expect("canonical package entry");
            assert!(!entry.is_symlink());
            assert_eq!((entry.external_attributes() >> 16) & 0o6000, 0);
        }
        let text = extract_word_document(canonical, WordFileType::Docx, 10 * 1024 * 1024)
            .expect("canonical package should still extract");
        assert_eq!(text, "Preserved Maple text");
    }

    #[test]
    fn rejects_doctype_in_docx_xml() {
        let malicious = r#"<!DOCTYPE w:document [<!ENTITY x "boom">]>
          <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:body><w:p><w:r><w:t>&x;</w:t></w:r></w:p></w:body>
          </w:document>"#;
        let bytes = zip_entries(&[
            ("[Content_Types].xml", content_types_xml()),
            ("_rels/.rels", package_rels_xml()),
            ("word/document.xml", malicious.to_string()),
        ]);
        let error = preflight_docx(&bytes, DOCX_LIMITS).expect_err("DOCTYPE should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_docx_part_over_a_configured_expansion_limit() {
        let bytes = minimal_docx("<w:p><w:r><w:t>hello</w:t></w:r></w:p>");
        let limits = DocxLimits {
            max_entry_bytes: 64,
            ..DOCX_LIMITS
        };
        let error = preflight_docx(&bytes, limits).expect_err("oversized part should fail");
        assert_eq!(error, WORD_COMPLEXITY_ERROR);
    }

    #[test]
    fn rejects_docx_before_zip_parsing_when_entry_count_exceeds_the_limit() {
        let bytes = minimal_docx("<w:p/>");
        let limits = DocxLimits {
            max_entries: 2,
            ..DOCX_LIMITS
        };
        let error = preflight_docx(&bytes, limits).expect_err("entry count should fail");
        assert_eq!(error, WORD_COMPLEXITY_ERROR);
    }

    #[test]
    fn rejects_docx_model_element_amplification() {
        let bytes = minimal_docx("<w:p><w:r><w:t>a</w:t><w:tab/><w:tab/><w:tab/></w:r></w:p>");
        let limits = DocxLimits {
            max_model_elements: 5,
            ..DOCX_LIMITS
        };
        let error = preflight_docx(&bytes, limits).expect_err("model budget should fail");
        assert_eq!(error, WORD_COMPLEXITY_ERROR);
    }

    #[test]
    fn rejects_non_utf8_docx_xml_declarations() {
        let document = document_xml("<w:p><w:r><w:t>hello</w:t></w:r></w:p>").replacen(
            "UTF-8",
            "windows-1252",
            1,
        );
        let bytes = zip_entries(&[
            ("[Content_Types].xml", content_types_xml()),
            ("_rels/.rels", package_rels_xml()),
            ("word/document.xml", document),
        ]);
        let error = preflight_docx(&bytes, DOCX_LIMITS).expect_err("encoding should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_descending_legacy_doc_character_positions() {
        let (mut word, table) = synthetic_legacy_streams(&[0, 10, 5], &[512, 532], false);
        word[0x4c..0x50].copy_from_slice(&20_u32.to_le_bytes());
        let error = validate_legacy_clx(&word, &table, 10 * 1024 * 1024)
            .expect_err("descending CP values should fail");
        assert_eq!(error, WORD_READ_ERROR);
    }

    #[test]
    fn rejects_too_many_legacy_doc_pieces() {
        let piece_count = MAX_LEGACY_DOC_PIECES + 1;
        let cps = (0..=piece_count as u32).collect::<Vec<_>>();
        let fcs = vec![512_u32; piece_count];
        let (word, table) = synthetic_legacy_streams(&cps, &fcs, true);
        let error = validate_legacy_clx(&word, &table, 10 * 1024 * 1024)
            .expect_err("piece count should fail");
        assert_eq!(error, WORD_COMPLEXITY_ERROR);
    }

    #[test]
    fn rejects_legacy_doc_decoded_output_over_budget() {
        let (mut word, table) = synthetic_legacy_streams(&[0, 4], &[512], true);
        word[512..516].fill(0x80);
        let error = validate_legacy_clx(&word, &table, 10)
            .expect_err("decoded UTF-8 output should exceed the budget");
        assert_eq!(error, WORD_COMPLEXITY_ERROR);
    }

    #[test]
    fn rejects_legacy_doc_repeated_source_amplification() {
        let (word, table) =
            synthetic_legacy_streams(&[0, 400, 800, 1_200], &[512, 512, 512], false);
        let error = validate_legacy_clx(&word, &table, 10 * 1024 * 1024)
            .expect_err("repeated source ranges should exceed the work budget");
        assert_eq!(error, WORD_COMPLEXITY_ERROR);
    }

    #[test]
    fn enforces_independent_extracted_text_limit() {
        let error = validate_extracted_text("Maple".repeat(3), 10)
            .expect_err("oversized extracted text should fail");
        assert!(error.contains("extracted text"), "{error}");
    }

    #[test]
    fn normalizes_percent_encoding_and_windows_separators_for_duplicate_checks() {
        assert_eq!(
            normalize_opc_entry_name(r"WORD%2fDOCUMENT.XML").expect("valid path"),
            "word/document.xml"
        );
        assert_eq!(
            normalize_opc_entry_name(r"word\document.xml").expect("valid path"),
            "word/document.xml"
        );
    }

    fn minimal_docx(body: &str) -> Vec<u8> {
        zip_entries(&[
            ("[Content_Types].xml", content_types_xml()),
            ("_rels/.rels", package_rels_xml()),
            ("word/document.xml", document_xml(body)),
        ])
    }

    fn legacy_doc_fixture() -> Vec<u8> {
        let encoded = include_str!("../tests/fixtures/nested_tables.doc.b64")
            .split_whitespace()
            .collect::<String>();
        BASE64.decode(encoded).expect("decode legacy DOC fixture")
    }

    fn synthetic_legacy_streams(
        character_positions: &[u32],
        file_offsets: &[u32],
        compressed: bool,
    ) -> (Vec<u8>, Vec<u8>) {
        assert_eq!(character_positions.len(), file_offsets.len() + 1);
        let mut plc = Vec::new();
        for position in character_positions {
            plc.extend_from_slice(&position.to_le_bytes());
        }
        for file_offset in file_offsets {
            plc.extend_from_slice(&0_u16.to_le_bytes());
            let encoded = if compressed {
                0x4000_0000 | file_offset.saturating_mul(2)
            } else {
                *file_offset
            };
            plc.extend_from_slice(&encoded.to_le_bytes());
            plc.extend_from_slice(&0_u16.to_le_bytes());
        }
        let mut clx = vec![0x02];
        clx.extend_from_slice(&(plc.len() as u32).to_le_bytes());
        clx.extend_from_slice(&plc);

        let mut word = vec![0_u8; 2_048];
        word[..2].copy_from_slice(&0xa5ec_u16.to_le_bytes());
        word[0x4c..0x50].copy_from_slice(&character_positions.last().unwrap_or(&0).to_le_bytes());
        word[0x01a2..0x01a6].copy_from_slice(&0_u32.to_le_bytes());
        word[0x01a6..0x01aa].copy_from_slice(&(clx.len() as u32).to_le_bytes());
        (word, clx)
    }

    fn zip_entries(entries: &[(&str, String)]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, contents) in entries {
            writer.start_file(*name, options).expect("start ZIP part");
            writer
                .write_all(contents.as_bytes())
                .expect("write ZIP part");
        }
        writer.finish().expect("finish ZIP").into_inner()
    }

    fn docx_with_symlink(name: &str, target: &str) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut entries = vec![
            ("[Content_Types].xml", content_types_xml()),
            ("_rels/.rels", package_rels_xml()),
        ];
        if name != "word/document.xml" {
            entries.push((
                "word/document.xml",
                document_xml("<w:p><w:r><w:t>Safe text</w:t></w:r></w:p>"),
            ));
        }
        for (entry_name, contents) in entries {
            writer
                .start_file(entry_name, options)
                .expect("start ZIP part");
            writer
                .write_all(contents.as_bytes())
                .expect("write ZIP part");
        }
        writer
            .add_symlink(name, target, SimpleFileOptions::default())
            .expect("add symbolic-link ZIP part");
        writer.finish().expect("finish ZIP").into_inner()
    }

    fn set_zip_entry_mode(bytes: &mut [u8], name: &str, mode: u32) {
        let end_of_directory = bytes.len() - 22;
        assert_eq!(
            &bytes[end_of_directory..end_of_directory + 4],
            b"PK\x05\x06"
        );
        let mut cursor = super::read_le_u32(bytes, end_of_directory + 16)
            .expect("central-directory offset") as usize;
        while cursor < end_of_directory {
            assert_eq!(&bytes[cursor..cursor + 4], b"PK\x01\x02");
            let name_len =
                super::read_le_u16(bytes, cursor + 28).expect("part name length") as usize;
            let extra_len = super::read_le_u16(bytes, cursor + 30).expect("extra length") as usize;
            let comment_len =
                super::read_le_u16(bytes, cursor + 32).expect("comment length") as usize;
            if &bytes[cursor + 46..cursor + 46 + name_len] == name.as_bytes() {
                bytes[cursor + 5] = 3; // Unix creator system.
                bytes[cursor + 38..cursor + 42].copy_from_slice(&(mode << 16).to_le_bytes());
                return;
            }
            cursor += 46 + name_len + extra_len + comment_len;
        }
        panic!("ZIP fixture has no part named {name}");
    }

    fn content_types_xml() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
          <Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
            <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
            <Default Extension="xml" ContentType="application/xml"/>
            <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
          </Types>"#
            .to_string()
    }

    fn package_rels_xml() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
          <Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
            <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
          </Relationships>"#
            .to_string()
    }

    fn document_xml(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
              <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                <w:body>{body}</w:body>
              </w:document>"#
        )
    }
}
