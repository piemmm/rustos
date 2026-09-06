//! The reading vocabulary every Switchboard surface states its figures in.
//!
//! Every reading is either a real measurement or an explicit statement that
//! there is none. A missing figure is carried as [`Unmeasured`], which names
//! *why* it is missing, so the surface can say "not permitted" where the
//! caller's authority stops and "unavailable" where the reading is permitted
//! but the service could not answer it — two facts a reader must never see
//! conflated into one blank.

use alloc::string::String;

use crate::sample::Absence;
use crate::view::UNMEASURED_READING;

/// Why a reading is not shown, in the words the surface uses.
///
/// The view renders this text beside the unmeasured mark, so a reader can
/// tell a refusal from a fault without opening a log. It is a display
/// vocabulary, not an authority decision: the service has already made
/// that decision and is reporting it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Unmeasured {
    /// The capability gating the reading is outside this session's
    /// ceiling, so the query was never issued and never will be.
    NotPermitted,
    /// The reading is permitted, but the service could not answer it.
    Unavailable,
    /// No interface exists to ask the question at all — no query, no
    /// syscall, no service. Distinct from the two above because neither a
    /// grant nor a retry would produce the figure.
    NoInterface,
}

impl Unmeasured {
    /// The short phrase the surface prints after the unmeasured mark.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Unmeasured::NotPermitted => "not permitted",
            Unmeasured::Unavailable => "unavailable",
            Unmeasured::NoInterface => "no interface",
        }
    }

    /// The display reason for an [`Absence`] the sample already resolved,
    /// so the view never re-derives a verdict the service reached.
    #[must_use]
    pub const fn from_absence(absence: Absence) -> Self {
        match absence {
            Absence::NotPermitted => Unmeasured::NotPermitted,
            Absence::Unavailable => Unmeasured::Unavailable,
        }
    }
}

/// A statement that `subject` is absent, in the one wording the whole
/// product uses.
///
/// Every screen has something it cannot show — a list with no registry
/// behind it, a block with no interface to ask, a reading outside this
/// session's ceiling — and each must say so rather than render an empty
/// space a reader would read as "nothing is wrong". Wording that once,
/// here, is what stops each section inventing its own phrasing for the
/// same refusal, and keeps "not permitted", "unavailable" and "no
/// interface" three visibly different statements.
#[must_use]
pub fn absence_statement(subject: &str, reason: Unmeasured) -> String {
    alloc::format!("{UNMEASURED_READING}: {subject} — {}", reason.reason())
}

/// What a detail pane says while its master list has nothing selected.
///
/// `subject` names the thing to pick, with its article ("a fault", "a
/// device"), so the sentence reads naturally in every section. Wording it
/// once here is what keeps two master/detail panes from each inventing
/// their own phrasing for the same empty state, and it is a prompt rather
/// than an absence: nothing is missing, the reader has simply not chosen
/// yet, so it never wears the unmeasured mark.
#[must_use]
pub fn selection_prompt(subject: &str) -> String {
    alloc::format!("Select {subject} to see its detail.")
}

/// A reading that is either measured or explicitly not.
///
/// Used wherever a figure may be missing, so no call site can accidentally
/// render an absent value as an empty string, a dash, or a fabricated
/// zero — the type makes the honest form the only reachable one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reading {
    /// A real measurement, already formatted for display.
    Measured(String),
    /// No measurement, and why.
    Absent(Unmeasured),
}

impl Reading {
    /// A measured reading from `text`.
    #[must_use]
    pub fn measured(text: impl Into<String>) -> Self {
        Reading::Measured(text.into())
    }

    /// A reading with no interface behind it at all.
    #[must_use]
    pub const fn no_interface() -> Self {
        Reading::Absent(Unmeasured::NoInterface)
    }

    /// The measured text, or [`None`] when the reading is absent.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Reading::Measured(text) => Some(text.as_str()),
            Reading::Absent(_) => None,
        }
    }

    /// Why this reading is absent, or [`None`] when it is measured.
    #[must_use]
    pub const fn absence(&self) -> Option<Unmeasured> {
        match self {
            Reading::Measured(_) => None,
            Reading::Absent(reason) => Some(*reason),
        }
    }
}

/// A reading as the text a fact or tile shows: the measurement itself, or
/// the unmeasured mark followed by why there is none.
///
/// The one place a [`Reading`] becomes display text, so every section that
/// carries one renders the same absent reading identically rather than each
/// spelling its own variant of the unmeasured mark.
#[must_use]
pub fn reading_text(reading: &Reading) -> String {
    match reading {
        Reading::Measured(text) => text.clone(),
        Reading::Absent(reason) => {
            alloc::format!("{UNMEASURED_READING} — {}", reason.reason())
        }
    }
}

/// One labelled fact: a name and the reading behind it.
///
/// The one row shape every fact list on this screen uses, so a fact that
/// is measured and one that is not are laid out identically and a reader's
/// eye does not have to re-learn the surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadingFact {
    /// What the fact is called (e.g. "Hostname").
    pub label: String,
    /// Its reading, measured or explicitly absent.
    pub value: Reading,
}

impl ReadingFact {
    /// A fact named `label` reading `value`.
    #[must_use]
    pub fn new(label: impl Into<String>, value: Reading) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }

    /// A fact whose value is plain text rather than a measurement — the
    /// parts of a surface that state a name.
    #[must_use]
    pub fn text(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(label, Reading::measured(value))
    }

    /// A fact with no interface behind its value at all.
    #[must_use]
    pub fn absent(label: impl Into<String>, reason: Unmeasured) -> Self {
        Self::new(label, Reading::Absent(reason))
    }
}

/// How badly a volume is faring, as the mount table reports it.
///
/// Its own three-state vocabulary rather than a task's recovery posture: a
/// disk is not a process, and borrowing a type whose other states can never
/// arise here would leave unreachable cases for a reader to puzzle over.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum HealthSeverity {
    /// The volume is available and reports no fault.
    #[default]
    Healthy,
    /// The volume is serving, but degraded or recovering.
    Degraded,
    /// The volume is unavailable: dirty, lost, or in recovery conflict.
    Failing,
}
