//! Deterministic sample data for tests and the GUI's UI test mode.

use crate::error::Result;
use crate::jmap::Identity;
use crate::store::{Address, Attachment, Classified, EmailBody, EmailSummary, Mailbox, Store};

/// Account the fixtures belong to: the id of a fresh config's only account.
pub const ACCOUNT: &str = "default";

/// Address of the fixture account owner.
const ME: &str = "me@example.com";

/// Address of the fixture account's second identity.
const SHOP: &str = "shop@example.com";

/// Mailboxes: id, name, role, sort order. The last four are the classifier's
/// tray mailboxes, present so a correction works offline.
const MAILBOXES: [(&str, &str, Option<&str>, i64); 13] = [
    ("mb-inbox", "Inbox", Some("inbox"), 1),
    ("mb-drafts", "Drafts", Some("drafts"), 2),
    ("mb-sent", "Sent", Some("sent"), 3),
    ("mb-archive", "Archive", Some("archive"), 4),
    ("mb-junk", "Junk", Some("junk"), 5),
    ("mb-trash", "Trash", Some("trash"), 6),
    ("mb-newsletters", "Newsletters", None, 10),
    ("mb-receipts", "Receipts", None, 11),
    ("mb-bilag", "Bilag", None, 12),
    ("mb-needs-reply", "Needs reply", None, 13),
    ("mb-updates", "Updates", None, 14),
    ("mb-promos", "Promos", None, 15),
    ("mb-social", "Social", None, 16),
];

/// One fixture email with its body.
struct Fx {
    /// Email id, `fx-NN`.
    id: &'static str,
    /// Received timestamp.
    at: &'static str,
    /// Sender name and address.
    from: (&'static str, &'static str),
    /// Subject line.
    subject: &'static str,
    /// Preview text.
    preview: &'static str,
    /// Mailbox ids.
    mailboxes: &'static [&'static str],
    /// JMAP keywords.
    keywords: &'static [&'static str],
    /// HTML body, if any.
    html: Option<&'static str>,
    /// Plain-text body, if any.
    text: Option<&'static str>,
}

/// The 14 fixture emails.
const EMAILS: [Fx; 14] = [
    Fx {
        id: "fx-01",
        at: "2026-10-09T08:15:00Z",
        from: ("Linear", "changelog@linear.example"),
        subject: "What's new in Linear — September",
        preview: "Cycles get a new planning view, and issues can now be \
                  linked across teams.",
        mailboxes: &["mb-inbox", "mb-newsletters"],
        keywords: &[],
        html: Some(
            "<html><body><h1>What's new in Linear</h1>\
             <p>Cycles get a new planning view.</p>\
             <img src=\"https://linear.example/track.png\">\
             <script>alert(1)</script>\
             <p><a href=\"https://linear.example/changelog\">Read more</a></p>\
             </body></html>",
        ),
        text: None,
    },
    Fx {
        id: "fx-02",
        at: "2026-10-08T19:40:00Z",
        from: ("Book club", "bookclub@example.org"),
        subject: "October pick: The Dispossessed",
        preview: "We meet on the 22nd at Hanne's place. Bring snacks and \
                  opinions.",
        mailboxes: &["mb-inbox"],
        keywords: &[],
        html: None,
        text: Some("We meet on the 22nd at Hanne's place.\nBring snacks and opinions.\n"),
    },
    Fx {
        id: "fx-03",
        at: "2026-10-07T06:30:00Z",
        from: ("Mailhost", "billing@mailhost.example"),
        subject: "Your invoice for October",
        preview: "Invoice 2026-10 for your Mailhost subscription is \
                  attached.",
        mailboxes: &["mb-inbox"],
        keywords: &["$flagged"],
        html: Some(
            "<html><body><p>Hello,</p><p>Invoice 2026-10 is attached.</p>\
             <p>Total due: <b>kr 149.00</b></p></body></html>",
        ),
        text: None,
    },
    Fx {
        id: "fx-04",
        at: "2026-10-04T05:00:00Z",
        from: ("The Sunday Long Read", "editor@sundaylongread.example"),
        subject: "The Sunday Long Read: the last lighthouse keeper",
        preview: "This week: a profile of the final manned lighthouse on \
                  the coast.",
        mailboxes: &["mb-newsletters"],
        keywords: &["$seen"],
        html: Some(
            "<html><body><h2>The last lighthouse keeper</h2>\
             <p>This week we visit the final manned lighthouse.</p></body></html>",
        ),
        text: None,
    },
    Fx {
        id: "fx-05",
        at: "2026-10-06T12:10:00Z",
        from: ("Ola Nordmann", "ola@example.net"),
        subject: "Lunch Thursday?",
        preview: "Are you free around noon? The new ramen place opened.",
        mailboxes: &["mb-inbox"],
        keywords: &["$seen"],
        html: None,
        text: Some("Are you free around noon on Thursday?\nThe new ramen place opened.\n"),
    },
    Fx {
        id: "fx-06",
        at: "2026-10-06T09:05:00Z",
        from: ("GitHub", "notifications@github.example"),
        subject: "[posto] CI passed on main",
        preview: "All 3 jobs succeeded for commit 582e667.",
        mailboxes: &["mb-inbox"],
        keywords: &[],
        html: None,
        text: Some("All 3 jobs succeeded for commit 582e667.\n"),
    },
    Fx {
        id: "fx-07",
        at: "2026-10-05T17:45:00Z",
        from: ("Sofie Berg", "sofie@example.net"),
        subject: "Weekend plans",
        preview: "Cabin is booked for the 17th. Can you drive?",
        mailboxes: &["mb-inbox"],
        keywords: &["$seen", "$flagged"],
        html: None,
        text: Some("Cabin is booked for the 17th.\nCan you drive?\n"),
    },
    Fx {
        id: "fx-08",
        at: "2026-09-27T05:00:00Z",
        from: ("The Sunday Long Read", "editor@sundaylongread.example"),
        subject: "The Sunday Long Read: how bread conquered the world",
        preview: "A short history of leavening, from sourdough to \
                  supermarket.",
        mailboxes: &["mb-newsletters"],
        keywords: &["$seen"],
        html: Some(
            "<html><body><h2>How bread conquered the world</h2>\
             <p>A short history of leavening.</p></body></html>",
        ),
        text: None,
    },
    Fx {
        id: "fx-09",
        at: "2026-09-25T13:20:00Z",
        from: ("Cafe Nero", "receipts@cafenero.example"),
        subject: "Your receipt from Cafe Nero",
        preview: "Thanks for your visit. Total: kr 78.00.",
        mailboxes: &["mb-receipts"],
        keywords: &["$seen"],
        html: None,
        text: Some("Thanks for your visit.\nTotal: kr 78.00.\n"),
    },
    Fx {
        id: "fx-10",
        at: "2026-09-22T10:00:00Z",
        from: ("Nordlys Supplies", "faktura@nordlys.example"),
        subject: "Faktura 2026-0412",
        preview: "Vedlagt faktura for septembers leveranse.",
        mailboxes: &["mb-bilag", "mb-archive"],
        keywords: &["$seen"],
        html: None,
        text: Some("Vedlagt faktura for septembers leveranse.\n"),
    },
    Fx {
        id: "fx-11",
        at: "2026-10-09T14:02:00Z",
        from: ("Birgitte Holm", "birgitte@holmrevision.example"),
        subject: "Q3 VAT — documents needed by the 25th",
        preview: "Could you send the sales ledger and the bank statements \
                  for Q3?",
        mailboxes: &["mb-inbox"],
        keywords: &["$flagged"],
        html: None,
        text: Some(
            "Hi,\n\nCould you send the sales ledger and the bank statements \
             for Q3? I need them by the 25th to file the VAT return.\n\n\
             Best,\nBirgitte\n",
        ),
    },
    Fx {
        id: "fx-12",
        at: "2026-09-20T09:00:00Z",
        from: ("Me", ME),
        subject: "Re: Q2 VAT",
        preview: "Attached are the Q2 documents. Let me know if anything \
                  is missing.",
        mailboxes: &["mb-sent"],
        keywords: &["$seen"],
        html: None,
        text: Some("Attached are the Q2 documents.\nLet me know if anything is missing.\n"),
    },
    Fx {
        id: "fx-13",
        at: "2026-09-30T22:15:00Z",
        from: ("Prize Desk", "winner@prizes.example"),
        subject: "You have won a free cruise!!!",
        preview: "Click here to claim your prize before midnight.",
        mailboxes: &["mb-junk"],
        keywords: &[],
        html: None,
        text: Some("Click here to claim your prize before midnight.\n"),
    },
    Fx {
        id: "fx-14",
        at: "2026-09-18T11:30:00Z",
        from: ("Old Service", "noreply@oldservice.example"),
        subject: "Your subscription has ended",
        preview: "We are sorry to see you go.",
        mailboxes: &["mb-trash"],
        keywords: &["$seen"],
        html: None,
        text: Some("We are sorry to see you go.\n"),
    },
];

/// Fills `store` with the fixture mailboxes, emails and bodies.
///
/// # Errors
///
/// Returns an error when a database write fails.
pub fn seed(store: &Store) -> Result<()> {
    let mut mailboxes: Vec<Mailbox> = MAILBOXES
        .iter()
        .map(|&(id, name, role, sort_order)| Mailbox {
            account_id: ACCOUNT.into(),
            id: id.into(),
            name: name.into(),
            role: role.map(String::from),
            parent_id: None,
            sort_order,
            total: 0,
            unread: 0,
        })
        .collect();

    store.upsert_mailboxes(&mailboxes)?;

    let me = Address {
        name: None,
        email: ME.into(),
    };
    let alias = Address {
        name: None,
        email: SHOP.into(),
    };
    store.set_identities(
        ACCOUNT,
        &[
            Identity {
                id: "id-1".into(),
                name: "Mikkel Malmberg".into(),
                email: ME.into(),
            },
            Identity {
                id: "id-2".into(),
                name: "Mikkel (shop)".into(),
                email: SHOP.into(),
            },
        ],
    )?;
    let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();

    for fx in &EMAILS {
        let num = fx.id.trim_start_matches("fx-");
        let body_len = fx.html.or(fx.text).map_or(0, str::len);
        let sent = fx.mailboxes.contains(&"mb-sent");
        let from = Address {
            name: (!sent).then(|| fx.from.0.to_string()),
            email: fx.from.1.into(),
        };
        let attachments = if fx.id == "fx-03" {
            vec![Attachment {
                blob_id: "blob-fx-03".into(),
                name: "Mailhost-inv-2026-10.pdf".into(),
                mime: "application/pdf".into(),
                size: 34000,
            }]
        } else {
            vec![]
        };
        let message_id = if fx.id == "fx-11" {
            "q3vat@holmrevision.example".to_string()
        } else {
            format!("{}@fixtures.posto.example", fx.id)
        };

        store.upsert_emails(&[EmailSummary {
            account_id: ACCOUNT.into(),
            id: fx.id.into(),
            thread_id: format!("th-{num}"),
            received_at: fx.at.into(),
            from: vec![from],
            to: vec![if fx.id == "fx-05" {
                alias.clone()
            } else {
                me.clone()
            }],
            cc: vec![],
            subject: fx.subject.into(),
            preview: fx.preview.into(),
            size: body_len as i64 + attachments.iter().map(|a| a.size).sum::<i64>(),
            has_attachment: !attachments.is_empty(),
            mailbox_ids: strings(fx.mailboxes),
            keywords: strings(fx.keywords),
        }])?;
        store.put_body(
            ACCOUNT,
            &EmailBody {
                email_id: fx.id.into(),
                html: fx.html.map(String::from),
                text: fx.text.map(String::from),
                attachments,
                in_reply_to: vec![],
                references: vec![],
                message_id: vec![message_id],
            },
        )?;
    }

    for (id, total, unread) in store.counts(ACCOUNT)? {
        if let Some(mb) = mailboxes.iter_mut().find(|m| m.id == id) {
            mb.total = total;
            mb.unread = unread;
        }
    }

    store.upsert_mailboxes(&mailboxes)?;
    seed_classifier(store)
}

/// Two classifications (`fx-01` sure, `fx-05` unsure) and one queued email
/// (`fx-11`), so the classifier views have something to show.
fn seed_classifier(store: &Store) -> Result<()> {
    let row = |id: &str, tray: &str, probs: [(&str, f64); 2], urgency, unsure| Classified {
        account_id: ACCOUNT.into(),
        email_id: id.into(),
        tray: tray.into(),
        probs: probs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        urgency,
        personal: 0.5,
        unsure,
        model: "jev-fixture".into(),
        input_tokens: 120,
        classified_at: "2026-10-09T08:20:00Z".into(),
        corrected_tray: None,
        applied: true,
        apply_error: None,
    };

    store.put_classification(&row(
        "fx-01",
        "promotional",
        [("promotional", 0.88), ("updates", 0.07)],
        1,
        false,
    ))?;
    store.put_classification(&row(
        "fx-05",
        "needs_reply",
        [("needs_reply", 0.46), ("social", 0.41)],
        3,
        true,
    ))?;

    store.enqueue_for_classification(ACCOUNT, &["fx-11".to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ListQuery;

    #[test]
    fn seed_matches_plan() {
        let store = Store::open_memory().unwrap();

        seed(&store).unwrap();

        assert_eq!(store.mailboxes(ACCOUNT).unwrap().len(), 13);
        assert_eq!(store.identities(ACCOUNT).unwrap().len(), 2);
        assert_eq!(
            store
                .list_emails(None, &ListQuery::default())
                .unwrap()
                .len(),
            14
        );
        assert_eq!(
            store.mailbox_by_role(ACCOUNT, "inbox").unwrap().unwrap().id,
            "mb-inbox"
        );
        assert!(
            store
                .email(ACCOUNT, "fx-03")
                .unwrap()
                .unwrap()
                .has_attachment
        );
        assert!(
            store
                .body(ACCOUNT, "fx-01")
                .unwrap()
                .unwrap()
                .html
                .unwrap()
                .contains("https://linear.example")
        );
        assert_eq!(
            store.body(ACCOUNT, "fx-11").unwrap().unwrap().message_id,
            ["q3vat@holmrevision.example"]
        );

        let inbox_unread = store
            .counts(ACCOUNT)
            .unwrap()
            .into_iter()
            .find(|(id, ..)| id == "mb-inbox")
            .map(|(_, _, unread)| unread)
            .unwrap();

        assert!(inbox_unread >= 5, "inbox unread = {inbox_unread}");

        let news = store
            .list_emails(
                Some(ACCOUNT),
                &ListQuery {
                    mailbox_id: Some("mb-newsletters".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert!(news.iter().all(|e| e.account_id == "default"));

        assert_eq!(news.len(), 3);

        let classified = store.classifications(Some(ACCOUNT), false, 0).unwrap();

        assert_eq!(classified.len(), 2);
        assert_eq!(
            store.classifications(None, true, 0).unwrap()[0].email_id,
            "fx-05"
        );
        assert_eq!(store.classify_queue_len(ACCOUNT).unwrap(), 1);
        assert_eq!(
            news.iter()
                .filter(|e| !e.mailbox_ids.contains(&"mb-inbox".to_string()))
                .count(),
            2
        );
    }
}
