# PDF extraction and OCR

Maple uses [`pdf_oxide` 0.3.74](https://crates.io/crates/pdf_oxide/0.3.74) for native PDF text extraction, page classification, rendering, and PaddleOCR preprocessing. PDF bytes and rendered pages stay on the device.

Maple pins [OpenSecretCloud's `pdf_oxide` maintenance commit](https://github.com/OpenSecretCloud/pdf_oxide/commit/f24b43ba997dd91ce60839640a8ce3ac92a87a5d) based directly on the published 0.3.74 release. The fork adds only a caller-controlled ONNX Runtime loader feature; it contains no Maple-specific parser or inference changes. This keeps iOS static linkage compatible without importing unpublished upstream work.

## One inference runtime

PDF OCR uses the Rust binding `ort = 2.0.0-rc.11` and Microsoft ONNX Runtime 1.23.2. Maple does not ship tract.

The runtime is part of the application package; users do not download it when opening a PDF:

| Platform | ONNX Runtime packaging |
| --- | --- |
| macOS | Microsoft's official Apple silicon and Intel dylibs, joined into one universal library and loaded from the app Frameworks directory. Minimum macOS is 13.4. |
| Linux | Microsoft's x64 or aarch64 shared library, installed under Maple's private library directory. |
| Windows | Microsoft's x64 DLL, installed next to `maple.exe` and loaded by its explicit path. |
| Android | Microsoft's official Android AAR, with `libonnxruntime.so` staged for arm64-v8a, armeabi-v7a, x86, and x86_64. |
| iOS | A deterministic device/simulator XCFramework built from the pinned Microsoft source commit and linked statically. |

Maple owns loader policy through PDFOxide's loader-neutral `ocr-ort` feature. Dynamic targets initialize the exact packaged library before OCR creates a session. iOS initializes the same Rust API against its static library. Cargo resolves exactly one `ort` version per target.

The application constructs and reuses one OCR engine. Native-only PDFs bypass model setup. OCR-routed pages are rendered once in full, bounded to a 2,000 by 2,000-pixel box, so tiled scans, inline images, and vector content do not depend on selecting one embedded image. Mixed pages retain native text and append OCR spans not already represented in the native layer. OCR remains optional enrichment for native-readable hybrid pages: if OCR is unavailable, Maple keeps the native text.

## Model download and cache

The first PDF that actually needs OCR downloads 12,577,821 bytes of PaddleOCR model data on macOS, Windows, Linux, iOS, or Android. The files are stored under the application cache at `ocr/models/paddleocr-en-v1`. An operating system may purge that cache, in which case Maple downloads and verifies the pack again.

Maple does not use PDFOxide's mutable model downloader. Every artifact is pinned to an immutable repository revision and checked for exact length and SHA-256 before loading:

| File | Immutable source | Bytes | SHA-256 |
| --- | --- | ---: | --- |
| `det.onnx` | [SWHL RapidOCR PaddleOCR detector](https://huggingface.co/SWHL/RapidOCR/blob/1cfba2e90fc938db55889873735088de210cc173/PP-OCRv4/ch_PP-OCRv4_det_infer.onnx) | 4,745,517 | `d2a7720d45a54257208b1e13e36a8479894cb74155a5efe29462512d42f49da9` |
| `rec.onnx` | [monkt English recognizer](https://huggingface.co/monkt/paddleocr-onnx/blob/7b02d0a30a07ba2b92ad1ff5a8941ae2c633de65/languages/english/rec.onnx) | 7,830,888 | `4e16deb22c4da6468bdca539b2cd3c8687825538b67109177c47d359ab994cd7` |
| `en_dict.txt` | [monkt English dictionary](https://huggingface.co/monkt/paddleocr-onnx/blob/7b02d0a30a07ba2b92ad1ff5a8941ae2c633de65/languages/english/dict.txt) | 1,416 | `e025a66d31f327ba0c232e03f407ae8d105e1e709e7ccb3f408aa778c24e70d6` |

Downloads stream to a temporary file, enforce the expected size, verify SHA-256, sync, and then rename into place. Concurrent first-use requests share one setup lock. Existing cached files are re-verified before engine construction. The backend emits download progress while the current UI keeps the attachment in its processing state.

PaddleOCR, SWHL RapidOCR, and the monkt model repository declare Apache-2.0. On Android, the model-only HTTP client uses rustls with Mozilla WebPKI roots; other platforms retain their normal trust-store integration.

## Panic and error boundary

All PDF parsing, classification, rendering, and inference work runs in an isolated blocking task. An ordinary Rust unwind becomes a normal Tauri command error, so the frontend clears its busy state and remains usable. Process aborts and native stack exhaustion cannot be recovered by a Rust unwind boundary.

The backend serializes PDF jobs, bounds OCR renders to four megapixels, and checks source-image count and pixel budgets. It independently enforces 10 MiB limits on both the input document and extracted text, rejects locked PDFs with a specific message, and treats a document with no recognized text as an error rather than silently attaching an empty document.

If OCR is required for a scanned page and that page cannot be processed, the upload fails with the page-specific error rather than attaching an incomplete subset of the document. OCR failures remain optional only for hybrid pages whose native text is already readable.

## Current scope and known limitation

The current model pack recognizes English text. Additional language packs, password entry, and a dedicated OCR download UI are separate product work.

PDFOxide 0.3.74 cannot render some uncompressed, one-bit FlateDecode, and inline PDF images ([upstream issue #860](https://github.com/yfedoseev/pdf_oxide/issues/860)). OCR sees a blank render for those pages even when Poppler displays the scan. A genuine ten-page NASA archival scan passes Maple's full-page OCR path, while a genuine ten-page NARA one-bit Flate scan reproduces #860. The loader-policy fork intentionally does not patch this parser/renderer issue in the initial viability change.

The MVP also caps each source image at 24 million pixels before PDFOxide decodes it. A typical 300-DPI archival scan fits; some 600-DPI scans do not, even though the final OCR render would be reduced to four million pixels. Maple reports the first affected page instead of attaching partial text. Raising the budget or adding decode-time downsampling requires separate desktop/mobile memory measurements.

## Local model-backed test

Provision the desktop runtime first, then run the ignored release-profile test with a real scanned PDF and the verified model pack:

```sh
cd apps/maple-research/frontend/src-tauri
./scripts/provide-macos-onnxruntime.sh # use the platform-equivalent provider

ORT_DYLIB_PATH=/absolute/path/from/the/provider \
MAPLE_OCR_TEST_PDF=/absolute/path/to/scanned.pdf \
MAPLE_OCR_MODEL_DIR=/absolute/path/to/models \
nix develop --no-update-lock-file -c cargo test --release --locked --lib \
  extracts_scanned_pdf_with_real_ocr_models -- --ignored --nocapture
```

Use the release profile for model-backed timing. Debug inference is not representative of the packaged application.
