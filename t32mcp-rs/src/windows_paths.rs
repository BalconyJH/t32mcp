// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

/// Windows device names remain reserved with an extension or trailing spaces.
pub(crate) fn is_device_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{00b9}"
                    | "\u{00b2}"
                    | "\u{00b3}"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_reserved_names_including_extensions_and_numbered_devices() {
        for name in [
            "con",
            "NUL.bin",
            "PRN .txt",
            "AUX",
            "CLOCK$",
            "CONIN$",
            "CONOUT$",
            "COM1",
            "com9.log",
            "LPT1",
            "lpt9.bin",
            "COM\u{00b9}",
            "LPT\u{00b2}",
            "COM\u{00b3}.bin",
        ] {
            assert!(is_device_name(name), "{name:?}");
        }
    }

    #[test]
    fn permits_ordinary_names_that_only_resemble_devices() {
        for name in [
            "",
            "console",
            "auxiliary.bin",
            "COM",
            "COM0",
            "COM10",
            "LPT0",
            "LPT10",
            "LPT\u{00b4}",
            "image.NUL",
            "my-COM1.bin",
        ] {
            assert!(!is_device_name(name), "{name:?}");
        }
    }
}
