# Markdown escaping for text that reaches a Matrix room through the Maubot
# webhook (see scripts/send-matrix-notification.sh).
#
# The webhook renders Markdown, and its Jinja template has autoescape on, so
# raw HTML is neutralised for us. Markdown is not -- the plugin's README says
# so and ships an `escape_md` filter for callers to apply themselves.
#
# This module exists so that escaping is defined once. It used to be pasted
# into each caller, and the first version missed three of the four ways a
# heading can be written, which is the kind of thing a second copy hides.
# `scripts/check-notify-escaping.sh` pins the invariant.

# Inline escape. Byte-equivalent to the plugin's own filter:
#   re.sub(r'([\\`*_[\]])', r'\\\1', value)
# Kills emphasis, code spans, and -- because `[` and `]` go -- links and
# images, which is what stops a body from forging a link whose label reads as
# a github.com URL and whose target is not.
def escape_md:
  gsub("(?<c>[\\\\`*_\\[\\]])"; "\\\(.c)");

# Block escape, applied per line so that `^` means what it looks like: jq
# anchors `^` to the start of the *string*, not the line, which is precisely
# the bug that shipped in the first version of this.
#
# Line endings are normalised to LF first, because "per line" has to mean the
# same thing here as it does in the renderer. CommonMark ends a line on LF,
# CRLF, or a lone CR (measured against the parser: nothing else counts, not
# U+2028, U+2029, FF, VT or NEL), and normalises them all before parsing.
# Splitting on "\n" alone left two holes: a CRLF body put a trailing CR on
# every line, so `===\r` failed the setext match and stayed unescaped, and a
# lone-CR body arrived as one long line this never split at all -- which
# defeats the ATX branch too, not just setext. Matching the renderer's own
# model closes the class; patching the end-anchor with `\r?$` would only have
# closed the first.
#
# Two shapes, both of which CommonMark turns into a heading:
#
#   ATX      up to three leading spaces, then one to six `#` followed by a
#            space, tab, or end of line. Four or more leading spaces is an
#            indented code block, and seven or more hashes is not a heading at
#            all, so neither forges anything and both are left alone. The
#            trailing condition matters for ordinary prose as much as for
#            safety: `#42 is fixed by this PR` opens no heading, and escaping
#            it would put a visible stray backslash in the notification.
#   setext   a line of only `=` or only `-` promotes the paragraph above it to
#            <h1>/<h2>. Escaping the first character breaks the underline; a
#            real bullet (`- item`) has content after the dash and so does not
#            match. A `---` thematic break is escaped too -- it is the same
#            shape, and losing a horizontal rule in a notification is a fair
#            price for not having to tell the two apart.
#
# Lists are deliberately still rendered: they forge no heading, and mangling
# them makes every ordinary issue body harder to read. Blockquotes never reach
# CommonMark at all -- Jinja autoescape turns a leading `>` into `&gt;` first --
# so there is nothing here to do about them either way.
def escape_blocks:
  gsub("\r\n?"; "\n")
  | split("\n")
  | map(
      if test("^ {0,3}#{1,6}([ \t]|$)") then
        sub("(?<p>^ {0,3})(?<h>#)"; "\(.p)\\\(.h)")
      elif test("^ {0,3}(=+|-+)[ \t]*$") then
        sub("(?<p>^ {0,3})(?<c>[=-])"; "\(.p)\\\(.c)")
      else
        .
      end
    )
  | join("\n");

# What untrusted text (issue and pull-request titles, bodies and authors) gets.
# Order matters: escape_md escapes backslashes, so it has to run before
# anything that adds them.
def escape_untrusted:
  escape_md | escape_blocks;
