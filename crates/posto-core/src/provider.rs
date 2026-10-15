//! Provider-neutral types shared by the JMAP and Proton back ends.
//!
//! The rest of the core talks to one connection type, `Connection` (added by
//! the provider seam), and never to `jmap::Client` or `proton::Session`
//! directly. The types here are the vocabulary of that contract.

use serde::{Deserialize, Serialize};

use crate::store::EmailSummary;

/// A sending identity: a JMAP `Identity` or a Proton address that may send.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Identity {
    /// Server-assigned id.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Sender address.
    pub email: String,
}

/// What an action changes on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Replace the mailbox set with this one mailbox.
    Mailbox(String),
    /// Leave `remove`, then join `add`.
    Patch {
        /// Mailbox ids to join.
        add: Vec<String>,
        /// Mailbox ids to leave.
        remove: Vec<String>,
    },
    /// Add (`true`) or remove (`false`) a keyword: `$seen`, `$flagged` or
    /// `$answered`.
    Keyword(&'static str, bool),
}

/// The server's answer to an update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Updated {
    /// Ids the server refused; they are neither mirrored nor counted.
    pub refused: Vec<String>,
}

/// One page of changes after a sync cursor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeltaPage {
    /// Created and updated emails, as the server has them now.
    pub emails: Vec<EmailSummary>,
    /// Ids the server destroyed.
    pub destroyed: Vec<String>,
    /// Cursor after this page; persisted together with the page.
    pub state: String,
    /// More pages follow: call again with `state`.
    pub more: bool,
    /// Mailboxes changed too: the caller runs the mailbox sync again.
    pub mailboxes_changed: bool,
}

/// What one delta call found.
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    /// Changes since the cursor.
    Page(DeltaPage),
    /// The server cannot report changes from that cursor: the caller clears
    /// the account's emails and runs an initial sync.
    Resync,
}
