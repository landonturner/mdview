# Navigation guide

You followed a link to get here — press `Backspace` (or `ctrl-o`) to go
straight back to the demo, position restored.

## How following works

Press `o` and every link on screen grows a little label. Press a label's
letter and mdview dispatches by target:

- another **markdown file** opens right here in the pager
- a **`#fragment`** jumps to that heading
- a **web link** opens in your browser
- anything else is handed to the OS

## Places to go from here

- Down into a subdirectory: [the key reference](reference/keys.md)
- Straight to a section of it: [search keys](reference/keys.md#searching)
- Back to the start: [the demo](demo.md), or its
  [table section](demo.md#tables)
- A fragment within this file: [dead ends](#dead-ends)
- Out to the web: [the mdview repo](https://github.com/landonturner/mdview)

## Dead ends

Errors stay in the status bar — the pager never bails:

- A file that does not exist: [ghost](missing.md)
- A heading that does not exist: [nowhere](#no-such-heading)

Follow either, watch the message, keep reading. The back stack only grows
when navigation actually happens, so failed jumps don't pollute history.
