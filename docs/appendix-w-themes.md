# Appendix W: Chat themes

A chat can have a shared theme: a background, an accent colour for its bubbles and buttons, and, in a group, a picture in place of its initial. Themes are for fun and for telling chats apart ("the Christmas group"). They carry nothing about anyone's messages.

## Who sets it

- **A chat with a contact:** either side. Both see the same theme.
- **A group:** its owners and admins (the roster's, Appendix F). A theme from anyone else is ignored.
- **Our notes:** ourselves only; it isn't sent.

The newest setting wins: each theme carries when it was chosen (`set_ms`), and a device keeps a theme only if it is newer than the one it has. A receiver caps a theme's time at its own clock, so one dated in the future can't hold off every later change. Taking a theme away is a theme too (the plain look, newer), so it reaches everyone the same way. Each change leaves a notice in the chat (Appendix C, notice kind 7): "Alice changed the chat's theme", "…the group's picture", "…took the chat's theme away".

Anyone can keep their own plain look in any chat, whatever its theme. That choice stays on their devices (history key 9) and is never sent.

## The theme

```
Theme      = { 0 => uint,          ; set_ms: when it was chosen
               1 => Background,
               ? 2 => uint,        ; accent, 0xAARRGGBB
               ? 3 => Picture }    ; groups only
Background = { 0 => 0 }                                   ; plain
           / { 0 => 1, 1 => pattern, 2 => ink, 3 => paper }
           / { 0 => 2, 2 => colour }
           / { 0 => 3, 2 => top, 3 => bottom }           ; a gradient
           / { 0 => 4, 4 => bstr }                        ; a photo
Picture    = { 0 => 0, 1 => tstr, 2 => colour }           ; an emoji on a round field
           / { 0 => 1, 3 => bstr }                        ; a photo
```

Colours are `0xAARRGGBB`. A background or picture of a kind a version doesn't know reads as none.

Photos must be JPEG, PNG or WebP, stripped of metadata by the sender (Appendix C, *Photos*) and at most 384 KiB for a background and 128 KiB for a picture; apps shrink them first. A receiver drops a theme whose photos are too large or of another kind, or whose emoji is empty, longer than 32 bytes, or holds control characters. Apps draw a photo background dimmed, so the messages stay readable.

Patterns are numbered (1 snow, 2 pines, 3 hearts, 4 confetti, 5 leaves, 6 waves, 7 stars, 8 dots). Each is a tile of simple shapes (circles, round-ended lines, polygons) in a unit square, which every app fills in the ink colour and repeats on the paper colour, so a pattern looks the same everywhere and costs a few bytes. Apps offer ready-made packs (Christmas, Winter, Halloween, Birthday, Love, Beach, Autumn, Night sky) that set a pattern, an accent and a picture in one tap. In the dark theme, apps deepen light background colours (keeping their tint) so text stays readable.

## How it travels

- **With a contact:** `Theme = { 0 => 34, 2 => bstr (the theme), 3 => bstr (conversation) }` (Appendix C), sent like `Edit`: tracked, to every device of the contact's account and of ours, held for one away and sealed for its mailboxes; only to devices whose `Hello` has feature bit 22 (2,097,152). A contact's theme loses any picture. From one of our devices, the conversation names the chat (a sibling's chat with us is our chat with it; groups never go this way). Since the newest wins, a late mailbox copy changes nothing.
- **In a group:** as group content kind 10 inside MLS (Appendix F), from an owner or admin. When one of our devices adds a member, it sends the group's theme again, unchanged, so the newcomer has it; members who had it keep theirs, since it isn't newer.
