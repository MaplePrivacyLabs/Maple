//! Leading BOM helpers from `utils/text.ts`, in UTF-16 code units.
use pi_ai::types::JsString;
pub fn split_bom(content: &JsString) -> (JsString, JsString) {
    if content.units().next() == Some(0xfeff) {
        ("\u{feff}".into(), content.slice(1, content.utf16_len()))
    } else {
        (JsString::default(), content.clone())
    }
}
pub fn strip_bom(content: &JsString) -> JsString {
    split_bom(content).1
}
