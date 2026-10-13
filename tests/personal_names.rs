//! Workboard is for anyone to run, so nothing in it may name one person's
//! setup: their logins, organizations, bot, user IDs or tailnet. This
//! fails when such a name appears in a tracked file, or a file's path,
//! outside the few places that name where this code lives: its
//! repository, its image and its app ID, which a build of your own
//! overrides (docs/run-your-own.md).

use std::{collections::HashSet, path::Path, process::Command};

/// What no tracked file may contain, compared without regard to case, and
/// what it is.
const FORBIDDEN: &[(&str, &str)] = &[
    (
        "marrero",
        "the operator's name, or a login, organization or bot made of it",
    ),
    ("1894385", "the operator's numeric GitHub user ID"),
    ("333156278", "the bot's numeric GitHub user ID"),
    ("336730605", "the forge organization's numeric GitHub ID"),
    ("redhat.com", "an employer's email domain"),
];

/// Where a `.ts.net` name may appear: only this placeholder tailnet, or a
/// name under it, such as hub.example.ts.net, or other.example.ts.net,
/// which tests refuse as another hub's.
const TAILNET: &str = ".ts.net";
const EXAMPLE_TAILNET: &str = "example.ts.net";

/// What may stay, in which file (`*` for any), and why. Each is the
/// identity of this repository, not of a person: a build of your own
/// replaces them by configuration.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "*",
        "jmarrero-bot/work-tracker-poc",
        "this repository: its links, homepage and image",
    ),
    (
        "*",
        "io.github.jmarrero_bot.Workboard",
        "the GNOME app's default ID (WORKBOARD_APP_ID, APP_ID)",
    ),
    (
        "gnome/data/io.github.jmarrero_bot.Workboard.metainfo.xml",
        "io.github.jmarrero_bot",
        "the app's developer ID",
    ),
    (
        "gnome/Makefile",
        "io.github.jmarrero_bot",
        "the developer ID the metainfo is written with, which DEVELOPER_ID replaces",
    ),
    (
        "README.md",
        "jmarrero-forge/tracker",
        "the epic this repository's work is tracked in",
    ),
    (
        "docs/release.md",
        "jmarrero/atomic-setup",
        "how this repository's own hub is deployed",
    ),
    (
        "docs/release.md",
        "jmarrero-forge/atomic-setup",
        "how this repository's own hub is deployed",
    ),
];

/// This file, which names what it looks for.
const SELF: &str = "tests/personal_names.rs";

/// The tracked files, as git lists them.
fn tracked(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .expect("this test runs git, in a checkout of the repository");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("tracked paths are UTF-8")
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// What `line` of `path` names that it shouldn't, once the allowed names
/// are taken out, noting which allowed names were used.
fn problems(path: &str, line: &str, used: &mut HashSet<&'static str>) -> Vec<String> {
    let mut text = line.to_lowercase();
    for (file, name, _) in ALLOWED {
        if (*file == "*" || *file == path) && text.contains(&name.to_lowercase()) {
            text = text.replace(&name.to_lowercase(), "");
            used.insert(*name);
        }
    }
    let mut found: Vec<String> = FORBIDDEN
        .iter()
        .filter(|(name, _)| text.contains(name))
        .map(|(name, what)| format!("{what} ({name})"))
        .collect();
    let mut rest = text.as_str();
    while let Some(at) = rest.find(TAILNET) {
        let end = at + TAILNET.len();
        let start = rest[..at]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            .map_or(0, |i| i + 1);
        let host = &rest[start..end];
        if host != EXAMPLE_TAILNET && !host.ends_with(&format!(".{EXAMPLE_TAILNET}")) {
            found.push(format!(
                "a tailnet name ({host}), where docs use the placeholder hub.{EXAMPLE_TAILNET}"
            ));
        }
        rest = &rest[end..];
    }
    found
}

#[test]
fn nothing_names_one_persons_setup() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut used = HashSet::new();
    let mut hits = vec![];
    for path in tracked(root) {
        if path == SELF {
            continue;
        }
        for what in problems(&path, &path, &mut used) {
            hits.push(format!("{path} (its path): {what}"));
        }
        // Binary files, such as images, are checked by path only.
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            for what in problems(&path, line, &mut used) {
                hits.push(format!("{path}:{}: {what}", n + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "Use configuration, or a placeholder such as YOUR-LOGIN, example-org, \
         example-bot or hub.{EXAMPLE_TAILNET}; only where this code lives may \
         stay, listed in ALLOWED in {SELF}:\n{}",
        hits.join("\n")
    );
    let unused: Vec<_> = ALLOWED
        .iter()
        .filter(|(_, name, _)| !used.contains(name))
        .map(|(file, name, _)| format!("{file}: {name}"))
        .collect();
    assert!(unused.is_empty(), "No longer needed in ALLOWED: {unused:?}");
}

#[test]
fn the_check_finds_what_it_should() {
    let mut used = HashSet::new();
    // (path, line, problems found)
    let cases = [
        (
            "docs/a.md",
            "see https://github.com/jmarrero-bot/work-tracker-poc",
            0,
        ),
        (
            "docs/a.md",
            "--bot-login jmarrero-bot --forge-org jmarrero-forge",
            1,
        ),
        ("docs/a.md", "watch JMarrero", 1),
        ("src/a.rs", "--github-user 1894385", 1),
        ("src/a.rs", "someone@redhat.com", 1),
        ("docs/a.md", "https://hub.example.ts.net/", 0),
        ("docs/a.md", "https://bot.tail1234.ts.net/", 1),
        ("docs/a.md", "hub.example.ts.net and bot.tail1234.ts.net", 1),
        ("docs/a.md", "https://badexample.ts.net/", 1),
        ("docs/a.md", "https://hub.badexample.ts.net/", 1),
        ("docs/a.md", "--allowed-host HUB.example.ts.net:8443", 0),
        ("docs/a.md", "Joseph Marrero", 1),
        ("src/a.rs", "org id 336730605", 1),
        ("README.md", "[jmarrero-forge/tracker#8]", 0),
        ("docs/a.md", "[jmarrero-forge/tracker#8]", 1),
        ("gnome/x.yml", "id: io.github.jmarrero_bot.Workboard", 0),
        ("gnome/x.yml", "developer: io.github.jmarrero_bot", 1),
    ];
    for (path, line, expected) in cases {
        assert_eq!(
            problems(path, line, &mut used).len(),
            expected,
            "{path}: {line}"
        );
    }
}
