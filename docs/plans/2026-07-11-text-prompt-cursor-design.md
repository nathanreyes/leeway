# Text Prompt Cursor Design

## Goal

Make preselected label and amount prompts behave like standard single-line text
inputs. Pressing Left collapses the initial selection to the beginning, while
pressing Right collapses it to the end. Editing then occurs at the visible cursor.

## Design

`TextPrompt` will keep a cursor position in addition to its existing buffer and
initial-selection state. The position will be a UTF-8 byte boundary so insertion,
deletion, movement, and rendering can use Rust string operations without splitting
a Unicode scalar value.

While the seeded value is selected:

- typing replaces the complete value and places the cursor after the new character;
- Backspace deletes the complete value and leaves the cursor at the beginning;
- Left preserves the value, clears the selection, and places the cursor at the
  beginning;
- Right preserves the value, clears the selection, and places the cursor at the end.

After the selection is cleared, Left and Right move by one Unicode scalar value,
typing inserts at the cursor, and Backspace deletes the preceding value. Empty and
new-value prompts use the same editing behavior with a cursor at the end.

The modal renderer will split the buffer at the cursor and draw the caret between
the two halves. The existing full-value highlight remains while the initial
selection is active.

## Scope

The behavior belongs to the shared text-prompt handler, so it applies consistently
to every preselected label and amount editor without changing individual call sites.
Series search remains unchanged because it is a distinct filtering control.

## Verification

Focused unit tests will cover collapsing a seeded selection to either edge,
inserting at both edges, moving within a Unicode value, deleting at the cursor, and
retaining the existing type-to-replace behavior. The full Rust test suite and
formatter will also run.
