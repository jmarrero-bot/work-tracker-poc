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
