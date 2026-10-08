//! Explicit applicability for the pinned source's three platform-gated path tests.
//! This is registration metadata, never evidence that assertions executed.
use crate::CheckResult;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AppliesTo {
    Windows,
    NonWindows,
}
impl AppliesTo {
    fn includes(self, windows: bool) -> bool {
        matches!(
            (self, windows),
            (Self::Windows, true) | (Self::NonWindows, false)
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SourceKind {
    EarlyReturn,
    RunIf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Platform {
    pub applies_to: AppliesTo,
    pub source_line: u64,
    pub source_kind: SourceKind,
}

const FILE: &str = "packages/coding-agent/test/paths.test.ts";
fn source_gate(id: &str, file: &str, line: u64) -> Option<(AppliesTo, u64, SourceKind)> {
    if file != FILE {
        return None;
    }
    let (suffix, declaration_line, applicability, guard_line, kind) = match line {
        109 => (
            "resolvePath > preserves POSIX absolute paths with literal percent sequences",
            109,
            AppliesTo::NonWindows,
            110,
            SourceKind::EarlyReturn,
        ),
        120 => (
            "resolvePath > does not treat Windows file URL pathname strings as native paths",
            120,
            AppliesTo::Windows,
            121,
            SourceKind::EarlyReturn,
        ),
        154 => (
            "normalizeWindowsShellPath > is applied by normal path handling on Windows",
            154,
            AppliesTo::Windows,
            154,
            SourceKind::RunIf,
        ),
        _ => return None,
    };
    (id == format!("{FILE} > {suffix}") && line == declaration_line).then_some((
        applicability,
        guard_line,
        kind,
    ))
}

/// Recognize only a first-statement `if [!]cfg!(windows) { return; }`.
/// Conditional test/module attributes remain prohibited by the existing checker.
pub(crate) fn body_guard(function: &syn::ItemFn) -> Option<AppliesTo> {
    let Some(syn::Stmt::Expr(syn::Expr::If(guard), _)) = function.block.stmts.first() else {
        return None;
    };
    if guard.else_branch.is_some()
        || !matches!(guard.then_branch.stmts.as_slice(), [syn::Stmt::Expr(syn::Expr::Return(ret), _)] if ret.expr.is_none())
    {
        return None;
    }
    let (condition, applies_to) = match guard.cond.as_ref() {
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Not(_)) => {
            (unary.expr.as_ref(), AppliesTo::Windows)
        }
        expression => (expression, AppliesTo::NonWindows),
    };
    let syn::Expr::Macro(mac) = condition else {
        return None;
    };
    (mac.mac.path.is_ident("cfg") && mac.mac.tokens.to_string() == "windows").then_some(applies_to)
}

pub(crate) fn validate(
    id: &str,
    file: &str,
    line: u64,
    platform: Option<&Platform>,
    guard: Option<AppliesTo>,
    windows: bool,
) -> CheckResult<bool> {
    let known = source_gate(id, file, line);
    match (known, platform) {
        (None, None) if guard.is_none() => Ok(true),
        (Some((expected, source_line, source_kind)), Some(platform))
            if platform.applies_to == expected
                && platform.source_line == source_line
                && platform.source_kind == source_kind
                && guard == Some(expected) =>
        {
            Ok(expected.includes(windows))
        }
        _ => Err(format!(
            "platform applicability for {id} needs its exact pinned source predicate, provenance, and unconditional body guard"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_both_platforms_without_claiming_execution() {
        for (line, title, applies_to, source_line, source_kind, guard) in [
            (
                109,
                "resolvePath > preserves POSIX absolute paths with literal percent sequences",
                AppliesTo::NonWindows,
                110,
                SourceKind::EarlyReturn,
                "if cfg!(windows) { return; }",
            ),
            (
                120,
                "resolvePath > does not treat Windows file URL pathname strings as native paths",
                AppliesTo::Windows,
                121,
                SourceKind::EarlyReturn,
                "if !cfg!(windows) { return; }",
            ),
            (
                154,
                "normalizeWindowsShellPath > is applied by normal path handling on Windows",
                AppliesTo::Windows,
                154,
                SourceKind::RunIf,
                "if !cfg!(windows) { return; }",
            ),
        ] {
            let id = format!("{FILE} > {title}");
            let function =
                syn::parse_str::<syn::ItemFn>(&format!("fn case() {{ {guard} assert!(true); }}"))
                    .unwrap();
            let platform = Platform {
                applies_to,
                source_line,
                source_kind,
            };
            for windows in [false, true] {
                assert_eq!(
                    validate(
                        &id,
                        FILE,
                        line,
                        Some(&platform),
                        body_guard(&function),
                        windows
                    )
                    .unwrap(),
                    applies_to.includes(windows)
                );
            }
            assert!(validate(&id, FILE, line, None, body_guard(&function), false).is_err());
            assert!(validate(&id, FILE, line, Some(&platform), None, false).is_err());
        }
    }

    #[test]
    fn rejects_new_ids_wrong_provenance_and_reversed_guards() {
        let id = format!(
            "{FILE} > resolvePath > does not treat Windows file URL pathname strings as native paths"
        );
        let mut platform = Platform {
            applies_to: AppliesTo::Windows,
            source_line: 121,
            source_kind: SourceKind::EarlyReturn,
        };
        assert!(
            validate(
                "new unchecked test",
                FILE,
                120,
                Some(&platform),
                Some(AppliesTo::Windows),
                false
            )
            .is_err()
        );
        assert!(
            validate(
                &id,
                FILE,
                120,
                Some(&platform),
                Some(AppliesTo::NonWindows),
                false
            )
            .is_err()
        );
        platform.source_line = 122;
        assert!(
            validate(
                &id,
                FILE,
                120,
                Some(&platform),
                Some(AppliesTo::Windows),
                false
            )
            .is_err()
        );
        assert!(
            validate(
                "ordinary test",
                FILE,
                1,
                None,
                Some(AppliesTo::Windows),
                false
            )
            .is_err()
        );
        assert!(validate("ordinary test", FILE, 1, None, None, false).unwrap());
    }

    #[test]
    fn guard_requires_exact_unconditional_return_shape() {
        for body in [
            "if cfg!(unix) { return; }",
            "if !cfg!(windows) { assert!(true); return; }",
            "if !cfg!(windows) { return; } else { return; }",
            "assert!(true); if !cfg!(windows) { return; }",
        ] {
            let function =
                syn::parse_str::<syn::ItemFn>(&format!("fn case() {{ {body} }}")).unwrap();
            assert_eq!(body_guard(&function), None);
        }
    }
}
