# Posto privacy policy

Effective 2026-10-20.

Posto is a desktop email client that runs only on the computer where you
install it. It has no server of its own. This page says what Posto does
with the data it touches.

## What Posto reads

With your permission, Posto connects to the email accounts you add
(Fastmail, Proton Mail or Gmail) and reads your mailboxes, labels,
messages, attachments and sending identities. It also moves, labels,
marks, deletes and sends mail when you ask it to.

## Where that data goes

- **Your computer.** Posto keeps a cache of your mail in a local database
  in your user data directory, so that it can show your mail quickly and
  search it offline. Sign-in tokens are stored in a local file that only
  your user account can read. Nothing is uploaded to Posto's author or to
  any Posto service, because there is none.
- **Your email providers.** Posto talks to Fastmail, Proton and Google
  only on your behalf, using the official APIs, to read and change your
  own mail.
- **The classifier, if you turn it on.** Posto can sort new inbox mail
  with Jev, a model served by TypeSafe. When this is enabled, the sender,
  subject and the first part of the body text of each new message are
  sent to the TypeSafe API, which returns a category. This is off until
  you add a TypeSafe API key. You can turn it off at any time by removing
  the key.

Posto does not send your data anywhere else, does not use it for
advertising, and does not sell or share it.

## Google user data

Posto's use of information received from Google APIs follows the
[Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy),
including the Limited Use requirements. Gmail data is used only to show
and manage your mail inside Posto. It is stored only in the local cache
on your computer, is never transferred to anyone other than the services
named above, and is never used for advertising or model training.

## Deleting your data

Remove an account in Posto's settings, or delete Posto's data directory,
and the cache and tokens for that account are gone. You can also revoke
Posto's access from your Google, Fastmail or Proton account settings.

## Contact

Open an issue at <https://github.com/photuris/posto/issues>.
