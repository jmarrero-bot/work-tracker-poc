//! What an item's native status means for the work: delivered, closed without
//! delivery, abandoned, still open, or unknown. Closed is not delivered, and an
//! item Workboard cannot read is never counted as done.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum Outcome {
    /// Open, in progress or waiting.
    #[default]
    Open,
    /// Merged, or resolved as done or fixed.
    Delivered,
    /// Closed or resolved without delivering anything. A human decides whether
    /// it was abandoned or needs rework.
    ClosedWithoutDelivery,
    /// A human decided this item will not deliver.
    Abandoned,
    /// Missing, inaccessible or unreadable.
    Unknown,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Delivered => "Delivered",
            Self::ClosedWithoutDelivery => "Closed, not delivered",
            Self::Abandoned => "Abandoned",
            Self::Unknown => "Unknown",
        }
    }
    /// Finished: nothing more to do or decide.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Delivered | Self::Abandoned)
    }
    /// Not progress: someone has to look at it.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::ClosedWithoutDelivery | Self::Unknown)
    }
    /// The reason recorded when a person sets a manual item's outcome.
    pub fn manual_reason(self) -> &'static str {
        match self {
            Self::Open => "Set manually as open",
            Self::Delivered => "Set manually as delivered",
            Self::ClosedWithoutDelivery => {
                "Set manually as closed without delivery; decide whether it was abandoned or needs rework"
            }
            Self::Abandoned => "Set manually as abandoned",
            Self::Unknown => "Set manually as unknown",
        }
    }
}

/// How far an effort is. Only delivered items are progress; closed without
/// delivery and unknown items need attention instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub delivered: usize,
    /// The linked items the effort still counts on: all but abandoned ones.
    pub total: usize,
    pub open: usize,
    pub needs_attention: usize,
    pub abandoned: usize,
}

impl Progress {
    pub fn of(outcomes: impl IntoIterator<Item = Outcome>) -> Self {
        let mut p = Self::default();
        for outcome in outcomes {
            match outcome {
                Outcome::Delivered => p.delivered += 1,
                Outcome::Open => p.open += 1,
                Outcome::ClosedWithoutDelivery | Outcome::Unknown => p.needs_attention += 1,
                Outcome::Abandoned => p.abandoned += 1,
            }
        }
        p.total = p.delivered + p.open + p.needs_attention;
        p
    }
    /// For example "1/3 delivered, 1 needs attention, 1 abandoned".
    pub fn summary(&self) -> String {
        let mut s = format!("{}/{} delivered", self.delivered, self.total);
        match self.needs_attention {
            0 => (),
            1 => s.push_str(", 1 needs attention"),
            n => s.push_str(&format!(", {n} need attention")),
        }
        if self.abandoned > 0 {
            s.push_str(&format!(", {} abandoned", self.abandoned));
        }
        s
    }
}

/// A derived outcome and a short explanation of why it was chosen.
pub type Derived = (Outcome, String);

/// GitLab merge request states, from the `state` field.
pub fn gitlab(state: &str) -> Derived {
    match state {
        "opened" => (Outcome::Open, "GitLab merge request is open".into()),
        // GitLab locks a merge request while it merges it.
        "locked" => (
            Outcome::Open,
            "GitLab merge request is locked, usually while merging; not delivered until merged"
                .into(),
        ),
        "merged" => (Outcome::Delivered, "GitLab merge request was merged".into()),
        "closed" => (
            Outcome::ClosedWithoutDelivery,
            "GitLab merge request was closed without merging; decide whether it was abandoned or needs rework".into(),
        ),
        other => (
            Outcome::Unknown,
            format!("GitLab reported an unrecognized merge request state '{other}'"),
        ),
    }
}

/// GitLab issue states. GitLab doesn't record whether a closed issue was done
/// or dropped, so a closed one is never taken as delivered.
pub fn gitlab_issue(state: &str) -> Derived {
    match state {
        "opened" => (Outcome::Open, "GitLab issue is open".into()),
        "closed" => (
            Outcome::ClosedWithoutDelivery,
            "GitLab issue was closed, and GitLab does not record whether it was done; decide whether it was abandoned or needs rework".into(),
        ),
        other => (
            Outcome::Unknown,
            format!("GitLab reported an unrecognized issue state '{other}'"),
        ),
    }
}

/// GitHub pull requests: merged is delivered, closed without merging isn't,
/// and a draft is open like any other.
pub fn github_pull_request(state: &str, merged: bool, draft: bool) -> Derived {
    match (state, merged) {
        (_, true) => (Outcome::Delivered, "GitHub pull request was merged".into()),
        ("open", _) if draft => (Outcome::Open, "GitHub pull request is a draft".into()),
        ("open", _) => (Outcome::Open, "GitHub pull request is open".into()),
        ("closed", _) => (
            Outcome::ClosedWithoutDelivery,
            "GitHub pull request was closed without merging; decide whether it was abandoned or needs rework".into(),
        ),
        (other, _) => (
            Outcome::Unknown,
            format!("GitHub reported an unrecognized pull request state '{other}'"),
        ),
    }
}

/// GitHub issues, from the state and the reason it was closed for. An issue
/// closed without a reason (closed before GitHub recorded them) is not taken
/// as delivered.
pub fn github_issue(state: &str, reason: Option<&str>) -> Derived {
    match (state, reason) {
        ("open", _) => (Outcome::Open, "GitHub issue is open".into()),
        ("closed", Some("completed")) => (
            Outcome::Delivered,
            "GitHub issue was closed as completed".into(),
        ),
        ("closed", Some(r @ ("not_planned" | "duplicate"))) => (
            Outcome::ClosedWithoutDelivery,
            format!(
                "GitHub issue was closed as {}; decide whether it was abandoned or needs rework",
                r.replace('_', " ")
            ),
        ),
        ("closed", None) => (
            Outcome::ClosedWithoutDelivery,
            "GitHub issue was closed without a recorded reason; decide whether it was abandoned or needs rework".into(),
        ),
        ("closed", Some(other)) => (
            Outcome::Unknown,
            format!("GitHub issue was closed for an unrecognized reason '{other}'"),
        ),
        (other, _) => (
            Outcome::Unknown,
            format!("GitHub reported an unrecognized issue state '{other}'"),
        ),
    }
}

/// What the pull request linked from a board item (its Branch) came to, as
/// far as it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linked {
    /// It has none, or its Branch isn't a pull request.
    Absent,
    Merged,
    NotMerged,
    /// It is missing, or hidden from the account.
    Unreadable,
}

/// The status on the bot's board that says the bot is done with an item.
pub const BOARD_DONE: &str = "Done";

/// An item on the bot's board, from its board status, what its own issue
/// or pull request came to (`content`, from [`github_issue`] or
/// [`github_pull_request`]), and its linked pull request. The board saying
/// Done is not delivery: only a merged pull request, its own or the linked
/// one, or its issue closed as completed is. Done otherwise needs a
/// person's decision.
pub fn bot_board_item(status: Option<&str>, content: Derived, linked: Linked) -> Derived {
    let board = status.unwrap_or("no status");
    let done = status == Some(BOARD_DONE);
    match (content.0, done, linked) {
        (Outcome::Delivered, _, _) => (
            Outcome::Delivered,
            format!("{}; the bot's board says {board}", content.1),
        ),
        (_, true, Linked::Merged) => (
            Outcome::Delivered,
            "Done on the bot's board, and its linked pull request was merged".into(),
        ),
        (_, true, Linked::Unreadable) => (
            Outcome::Unknown,
            "Done on the bot's board, but its linked pull request can't be read".into(),
        ),
        (_, true, Linked::Absent | Linked::NotMerged) => (
            Outcome::ClosedWithoutDelivery,
            "Done on the bot's board, but no pull request of it was merged and its issue wasn't closed as completed; decide whether it was abandoned or needs rework".into(),
        ),
        (Outcome::Open, false, _) => (Outcome::Open, format!("{board} on the bot's board")),
        (outcome, false, _) => (
            outcome,
            format!("{}; the bot's board still says {board}", content.1),
        ),
    }
}

/// Forgejo pull requests, such as Fedora Forge's: merged is delivered,
/// closed without merging isn't, and a draft (a work-in-progress title) is
/// open like any other.
pub fn forgejo_pull_request(state: &str, merged: bool, draft: bool) -> Derived {
    match (state, merged) {
        (_, true) => (Outcome::Delivered, "Forgejo pull request was merged".into()),
        ("open", _) if draft => (
            Outcome::Open,
            "Forgejo pull request is a draft (work in progress)".into(),
        ),
        ("open", _) => (Outcome::Open, "Forgejo pull request is open".into()),
        ("closed", _) => (
            Outcome::ClosedWithoutDelivery,
            "Forgejo pull request was closed without merging; decide whether it was abandoned or needs rework".into(),
        ),
        (other, _) => (
            Outcome::Unknown,
            format!("Forgejo reported an unrecognized pull request state '{other}'"),
        ),
    }
}

/// Forgejo issues. Forgejo doesn't record whether a closed issue was done
/// or dropped, so, as for GitLab, a closed one is never taken as delivered.
pub fn forgejo_issue(state: &str) -> Derived {
    match state {
        "open" => (Outcome::Open, "Forgejo issue is open".into()),
        "closed" => (
            Outcome::ClosedWithoutDelivery,
            "Forgejo issue was closed, and Forgejo does not record whether it was done; decide whether it was abandoned or needs rework".into(),
        ),
        other => (
            Outcome::Unknown,
            format!("Forgejo reported an unrecognized issue state '{other}'"),
        ),
    }
}

/// Jira resolutions that mean the work was delivered, compared
/// case-insensitively. Any other resolution (Won't Do, Duplicate, Not a Bug,
/// Obsolete, ...) is closed without delivery.
const JIRA_DELIVERED_RESOLUTIONS: &[&str] = &[
    "done",
    "done-errata",
    "fixed",
    "complete",
    "completed",
    "implemented",
    "delivered",
    "resolved",
];

/// Jira issues, from the status category key, plus the resolution name where
/// the issue has one. The category decides, never the status display name.
pub fn jira(status: &str, category: Option<&str>, resolution: Option<&str>) -> Derived {
    match category {
        Some("new") => (
            Outcome::Open,
            format!("Jira status '{status}' is in the To Do category"),
        ),
        Some("indeterminate") => (
            Outcome::Open,
            format!("Jira status '{status}' is in the In Progress category"),
        ),
        Some("done") => match resolution.map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) if JIRA_DELIVERED_RESOLUTIONS.contains(&r.to_lowercase().as_str()) => {
                (Outcome::Delivered, format!("Jira issue resolved as '{r}'"))
            }
            Some(r) => (
                Outcome::ClosedWithoutDelivery,
                format!("Jira issue resolved as '{r}', which is not a delivery; decide whether it was abandoned or needs rework"),
            ),
            None => (
                Outcome::Delivered,
                format!("Jira status '{status}' is in the Done category and the issue has no resolution"),
            ),
        },
        Some(other) => (
            Outcome::Unknown,
            format!("Jira status '{status}' has an unrecognized category '{other}'"),
        ),
        None => (
            Outcome::Unknown,
            format!("Jira did not report a category for status '{status}'"),
        ),
    }
}

/// A tracked item its source no longer returns: deleted, moved, or no longer
/// visible to the account, as the connector's `message` explains. Its last
/// known status stays visible.
pub fn inaccessible(message: &str) -> Derived {
    (
        Outcome::Unknown,
        format!("{message}; its last known status is shown"),
    )
}

/// Rows written before outcomes existed only had a `terminal` flag. Derive
/// their outcome from what was stored: synced items through their connector's
/// mapping, manual items from the status text the prototype treated as
/// complete. A finished Jira issue's resolution was never fetched, so it is
/// unknown until the next sync fetches it again.
pub fn legacy(provider: &str, synced: bool, status: &str, terminal: bool) -> Derived {
    if synced && crate::model::is_gitlab(provider) {
        // Draft was only shown for open merge requests.
        return gitlab(if status == "Draft" { "opened" } else { status });
    }
    if synced && provider == "jira" {
        return if terminal {
            (
                Outcome::Unknown,
                format!("Jira status '{status}' is in the Done category, but its resolution was not recorded before outcomes were tracked; checked again on the next sync"),
            )
        } else {
            (
                Outcome::Open,
                format!("Jira status '{status}' was not in the Done category"),
            )
        };
    }
    match status.to_lowercase().as_str() {
        "merged" | "done" | "released" => (
            Outcome::Delivered,
            format!("Manual status '{status}' was treated as complete before outcomes were tracked"),
        ),
        "closed" => (
            Outcome::ClosedWithoutDelivery,
            format!("Manual status '{status}' does not say whether the work was delivered; decide whether it was abandoned or needs rework"),
        ),
        _ => (
            Outcome::Open,
            format!("Manual status '{status}' was not complete"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Outcome::*;

    #[test]
    fn gitlab_states() {
        for (state, expected) in [
            ("opened", Open),
            ("locked", Open),
            ("merged", Delivered),
            ("closed", ClosedWithoutDelivery),
            ("archived", Unknown),
            ("", Unknown),
        ] {
            let (outcome, reason) = gitlab(state);
            assert_eq!(outcome, expected, "{state}");
            assert!(!reason.is_empty());
        }
    }

    #[test]
    fn gitlab_issue_states() {
        for (state, expected) in [
            ("opened", Open),
            // Closed is not delivered: GitLab doesn't say how.
            ("closed", ClosedWithoutDelivery),
            ("locked", Unknown),
        ] {
            assert_eq!(gitlab_issue(state).0, expected, "{state}");
        }
    }

    #[test]
    fn github_pull_requests() {
        for (state, merged, draft, expected) in [
            ("open", false, false, Open),
            ("open", false, true, Open),
            ("closed", true, false, Delivered),
            ("closed", false, false, ClosedWithoutDelivery),
            ("closed", false, true, ClosedWithoutDelivery),
            ("locked", false, false, Unknown),
        ] {
            let (outcome, reason) = github_pull_request(state, merged, draft);
            assert_eq!(outcome, expected, "{state} merged={merged} draft={draft}");
            assert!(reason.contains("pull request"), "{reason}");
        }
    }

    #[test]
    fn github_issues() {
        for (state, reason, expected) in [
            ("open", None, Open),
            ("open", Some("reopened"), Open),
            ("closed", Some("completed"), Delivered),
            ("closed", Some("not_planned"), ClosedWithoutDelivery),
            ("closed", Some("duplicate"), ClosedWithoutDelivery),
            // Closed is not delivered without a reason saying so.
            ("closed", None, ClosedWithoutDelivery),
            ("closed", Some("spam"), Unknown),
            ("deleted", None, Unknown),
        ] {
            assert_eq!(
                github_issue(state, reason).0,
                expected,
                "{state} {reason:?}"
            );
        }
    }

    #[test]
    fn bot_board_items_are_delivered_only_by_a_merge_or_a_completed_issue() {
        use Linked::*;
        use Outcome::*;
        let open_issue = || github_issue("open", None);
        let completed = || github_issue("closed", Some("completed"));
        let not_planned = || github_issue("closed", Some("not_planned"));
        let merged_pr = || github_pull_request("merged", true, false);
        let closed_pr = || github_pull_request("closed", false, false);
        // (board status, its own issue or pull request, linked, outcome)
        let cases = [
            (Some("Done"), merged_pr(), Absent, Delivered),
            (Some("Done"), completed(), Absent, Delivered),
            (Some("Done"), open_issue(), Merged, Delivered),
            (Some("Done"), not_planned(), Merged, Delivered),
            // Closed is not delivered, and neither is the board saying Done.
            (Some("Done"), open_issue(), Absent, ClosedWithoutDelivery),
            (
                Some("Done"),
                not_planned(),
                NotMerged,
                ClosedWithoutDelivery,
            ),
            (Some("Done"), closed_pr(), Absent, ClosedWithoutDelivery),
            (Some("Done"), open_issue(), Unreadable, Unknown),
            // The work landed, whatever the board still says.
            (Some("Draft"), merged_pr(), Absent, Delivered),
            (None, completed(), Absent, Delivered),
            // Not done: a merged Branch alone doesn't finish the item.
            (Some("In Review"), open_issue(), Merged, Open),
            (Some("Needs human"), open_issue(), Absent, Open),
            (None, open_issue(), Absent, Open),
            (
                Some("In Progress"),
                not_planned(),
                Absent,
                ClosedWithoutDelivery,
            ),
            (Some("Draft"), closed_pr(), Absent, ClosedWithoutDelivery),
        ];
        for (n, (status, content, linked, outcome)) in cases.into_iter().enumerate() {
            let (got, reason) = bot_board_item(status, content, linked);
            assert_eq!(got, outcome, "case {n}: {reason}");
            assert!(!reason.is_empty(), "case {n}");
        }
        assert_eq!(
            bot_board_item(Some("Needs human"), open_issue(), Absent).1,
            "Needs human on the bot's board"
        );
    }

    #[test]
    fn forgejo_pull_requests() {
        for (state, merged, draft, expected) in [
            ("open", false, false, Open),
            ("open", false, true, Open),
            ("closed", true, false, Delivered),
            ("closed", false, false, ClosedWithoutDelivery),
            ("closed", false, true, ClosedWithoutDelivery),
            ("locked", false, false, Unknown),
        ] {
            let (outcome, reason) = forgejo_pull_request(state, merged, draft);
            assert_eq!(outcome, expected, "{state} merged={merged} draft={draft}");
            assert!(reason.contains("pull request"), "{reason}");
        }
    }

    #[test]
    fn forgejo_issues() {
        for (state, expected) in [
            ("open", Open),
            // Closed is not delivered: Forgejo doesn't say how.
            ("closed", ClosedWithoutDelivery),
            ("locked", Unknown),
        ] {
            assert_eq!(forgejo_issue(state).0, expected, "{state}");
        }
    }

    #[test]
    fn jira_category_and_resolution() {
        for (status, category, resolution, expected) in [
            ("New", Some("new"), None, Open),
            ("In Progress", Some("indeterminate"), None, Open),
            // The display name never decides.
            ("Released", Some("indeterminate"), None, Open),
            ("Closed", Some("done"), Some("Done"), Delivered),
            ("Closed", Some("done"), Some("Fixed"), Delivered),
            ("Closed", Some("done"), Some("Done-Errata"), Delivered),
            ("Resolved", Some("done"), Some("done"), Delivered),
            (
                "Closed",
                Some("done"),
                Some("Won't Do"),
                ClosedWithoutDelivery,
            ),
            (
                "Closed",
                Some("done"),
                Some("Not planned"),
                ClosedWithoutDelivery,
            ),
            (
                "Closed",
                Some("done"),
                Some("Duplicate"),
                ClosedWithoutDelivery,
            ),
            (
                "Closed",
                Some("done"),
                Some("Obsolete"),
                ClosedWithoutDelivery,
            ),
            (
                "Closed",
                Some("done"),
                Some("Not a Bug"),
                ClosedWithoutDelivery,
            ),
            // No resolution: the Done category decides.
            ("Closed", Some("done"), None, Delivered),
            ("Closed", Some("done"), Some(" "), Delivered),
            ("Odd", Some("undefined"), None, Unknown),
            ("Odd", None, Some("Done"), Unknown),
        ] {
            let (outcome, reason) = jira(status, category, resolution);
            assert_eq!(outcome, expected, "{status} {category:?} {resolution:?}");
            assert!(reason.contains(status) || resolution.is_some_and(|r| reason.contains(r)));
        }
    }

    #[test]
    fn legacy_rows() {
        for (provider, synced, status, terminal, expected) in [
            ("gitlab-internal", true, "merged", true, Delivered),
            ("gitlab-public", true, "closed", true, ClosedWithoutDelivery),
            ("gitlab-internal", true, "opened", false, Open),
            ("gitlab-internal", true, "Draft", false, Open),
            ("gitlab-internal", true, "locked", false, Open),
            // Refreshed on the next sync, which fetches the resolution.
            ("jira", true, "Closed", true, Unknown),
            ("jira", true, "In Progress", false, Open),
            ("github", false, "Merged", true, Delivered),
            ("errata", false, "Released", true, Delivered),
            ("jira", false, "Done", true, Delivered),
            ("github", false, "Closed", true, ClosedWithoutDelivery),
            ("fedora", false, "Checks failed", false, Open),
            // A manual item named after a connector is still manual.
            ("gitlab-public", false, "Open", false, Open),
        ] {
            assert_eq!(
                legacy(provider, synced, status, terminal).0,
                expected,
                "{provider} {status}"
            );
        }
    }

    #[test]
    fn effort_progress_counts_only_delivered() {
        for (outcomes, (delivered, total, open, attention, abandoned), summary) in [
            (vec![], (0, 0, 0, 0, 0), "0/0 delivered"),
            (vec![Delivered, Open], (1, 2, 1, 0, 0), "1/2 delivered"),
            (
                vec![Delivered, ClosedWithoutDelivery],
                (1, 2, 0, 1, 0),
                "1/2 delivered, 1 needs attention",
            ),
            (
                vec![Unknown, ClosedWithoutDelivery, Open],
                (0, 3, 1, 2, 0),
                "0/3 delivered, 2 need attention",
            ),
            // Abandoned items are left out of the total.
            (
                vec![Delivered, Abandoned, ClosedWithoutDelivery],
                (1, 2, 0, 1, 1),
                "1/2 delivered, 1 needs attention, 1 abandoned",
            ),
            (
                vec![Abandoned],
                (0, 0, 0, 0, 1),
                "0/0 delivered, 1 abandoned",
            ),
        ] {
            let p = Progress::of(outcomes.iter().copied());
            assert_eq!(
                (p.delivered, p.total, p.open, p.needs_attention, p.abandoned),
                (delivered, total, open, attention, abandoned),
                "{outcomes:?}"
            );
            assert_eq!(p.summary(), summary);
        }
    }

    #[test]
    fn attention_and_settled_are_disjoint() {
        for outcome in [Open, Delivered, ClosedWithoutDelivery, Abandoned, Unknown] {
            assert!(!(outcome.is_settled() && outcome.needs_attention()));
        }
    }
}
