const DEVELOPMENT_B422D5F_BUNDLE: &[u8] = include_bytes!(
    "../../assets/release/development/b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc.json"
);
const DEVELOPMENT_B422D5F_PCR0_HEX: &str =
    "b422d5fcb25929a1b76f0eb8aa10c26a78c635a86a58039b48f22eb0dc4dc6f8ae6e646aa2fbbc57db756e6f06cad3dc";

pub(super) fn embedded_development_bundle(pcr0_hex: &str) -> Option<&'static [u8]> {
    (pcr0_hex == DEVELOPMENT_B422D5F_PCR0_HEX).then_some(DEVELOPMENT_B422D5F_BUNDLE)
}
