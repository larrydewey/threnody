# Appendix V: Cards

## Status

Built in `threnody_core::card` (the format), `threnody_net` (sending, receiving, view-once) and `threnody_ffi::cards` (the apps' interface).

## What a card is

A card is a typed item a text carries, which apps draw as a card with its own actions instead of as text:

- **Contact.** Someone to get in touch with: on Threnody (their invite link or handle, Appendix S), or by phone, email or post. Apps offer *Add* (for an invite link or handle) and *Save to contacts*.
- **Wi-Fi.** A network's name, password and security. Apps offer *Join* and *Copy password*.
- **Location.** A place (WGS 84, to 10⁻⁷ degrees) with an optional label and accuracy. Apps offer *Open in maps*.
- **Event.** A title, start, optional end, all-day flag, place and note. Apps offer *Add to calendar*.
- **Secret.** A login, a payment card, a secure note or another item, as from a password manager: a title and named fields, some marked hidden (shown covered until revealed). Apps offer *Copy* per field and, when its `source` is `symbolon`, *Save to Symbolon*.

## Wire format

A card travels as `Text` key 14 (Appendix C) in a chat with a contact, and as group `Text` key 15 (Appendix F) in a group, as its own CBOR (byte string):

```
card     = { 0: kind, ? 1: flags, ? 2: expires_in_s, 3: item }
kind     = 1 contact | 2 wifi | 3 location | 4 event | 5 secret
flags    = 1 view once | 2 no forwarding
contact  = { 0: name, ? 1: invite link, ? 2: handle, ? 3: [* phone],
             ? 4: [* email], ? 5: address, ? 6: note }
wifi     = { 0: ssid, ? 1: password, 2: security (0 open, 1 WPA/WPA2/WPA3 Personal, 2 WEP), ? 3: hidden }
location = { 0: latitude e7 (int), 1: longitude e7 (int), ? 2: label, ? 3: accuracy m }
event    = { 0: title, 1: start ms, ? 2: end ms, ? 3: all day, ? 4: place, ? 5: note }
secret   = { 0: secret kind (0 other, 1 login, 2 payment card, 3 note), 1: title,
             2: [* [name, value, hidden]], ? 3: source }
```

The text's body is the card's **fallback text**: what an app without cards shows, made from the card by the sender's node (`Card::fallback_text`). A contact's fallback lists its name, numbers, addresses and link; a Wi-Fi network's, its name and password; a place's, its coordinates; an event's, its title and start in UTC. A secret's says only its title and that it was sent securely: **a secret's contents never travel as plain text**. A view-once card's fallback says only that there is one. A text with a card carries no link preview.

Field names of a secret that apps know: `username`, `password`, `url`, `otp` (an `otpauth://` URI), `number`, `holder`, `expiry`, `cvc`, `pin`, `text`. Others are shown under their own name.

## Receiving

Receivers keep a card only if it is well formed (`Card::clean`); otherwise the text stands alone, as from an older peer:

- Every text is trimmed and cut to its limit (names 100 characters, notes and addresses 500, a secret's values 2,000), with control characters and bidirectional overrides removed. Names and labels lose their line breaks.
- Lists keep at most 5 phones or emails; a secret keeps at most 16 non-empty fields.
- An invite link or handle that doesn't parse is dropped, so a card can't pass off a web address as one.
- A contact needs a name and at least one way to reach them; a Wi-Fi network, a name of at most 32 bytes; a place, coordinates on the globe; an event, a title and a start; a secret, a title and a field.
- Unknown flags are dropped, a secret is always marked *no forwarding*, and a card's own time is at most a week.

History keeps the card with the entry (history key 22, the same encoding).

## Secrets, view once, no forwarding

- **Who may get them.** A secret or a view-once card goes only to one contact, and only if every device of theirs said in its `Hello` that it shows cards (`FEATURE_CARDS`, 1 048 576). An older app would show the fallback, or keep the card for good. Sending one to a group fails.
- **The sender's copy.** The sender's own history keeps only what it was: a secret's kind and title, or a view-once card's kind. What it held is not stored on the sending device, unless the user asks to keep what they send (`set_keep_sent_secrets`, off by default): then the sending device keeps the whole card, to open as often as it likes, still out of backups. What the receiver gets doesn't change.
- **View once.** The receiver's history entry shows the card's kind but not its contents. Opening it (`open_card`) gives the contents once and empties the stored card in the same step, so it can't be opened again. The receiver's other devices get their own copy and each can open it once.
- **No forwarding.** Apps offer no *Forward* for a card marked so, for a secret, or for a view-once card.
- **A time of its own.** A card may say how long it stays (`expires_in_s`, at most a week; none means for good): it then disappears like a disappearing message, at that time or the chat's timer, whichever comes first. Unlike a disappearing message, it doesn't change the chat's timer.
- **Kept out of copies.** Backups leave out secrets and view-once cards, as they leave out disappearing messages (Appendix C, *Backups*). Apps copy a secret's fields as sensitive (kept out of keyboard clipboard history where the platform allows) and show hidden fields covered until revealed.

## Privacy

- A contact card with someone's invite link tells the receiver how to reach them, as forwarding the link would. Apps fill a card with the user's own invite (*Share my contact*) or a handle the person published; they don't build a card from another contact's identity, which would introduce them without their consent.
- A location card says where the sender chose. Apps don't attach the device's position unless the user picks *Current location*.
- Opening a place in a map app or a link from a card is the user's choice; nothing is fetched to draw a card.
