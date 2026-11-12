// The template of the hizukuri release report: a deck of 16:9 slides that is
// read on a screen, one release per deck.
//
// A deck is one Typst file that imports this one:
//
//   #import "lib/hizukuri-slides.typ": *
//   #show: deck.with(release: "release-20270101-1", ...)
//   #title-slide()
//   #summary-section()
//   #slide(title: [今回のリリースの要点])[ ... ]
//   #item-list()
//   #item(kind: "追加", area: "検証用の compositor", title: [...], refs: (...))
//   #slide(title: [...])[ ... ]
//   #verify-section()
//   #slide(title: [...])[ ... ]
//
// Only fonts that the `slides` shell of flake.nix provides are named here.

// ---------------------------------------------------------------- palette

#let ink = rgb("#1d2228")
#let ink-soft = rgb("#4a535e")
#let ink-faint = rgb("#8a939e")
#let rule = rgb("#d9dde2")
#let paper = rgb("#ffffff")
#let wash = rgb("#f4f5f7")
#let ember = rgb("#c8432b")
#let ember-wash = rgb("#fbeeea")
#let night = rgb("#1b2530")

// The kinds of change an item can be, and the color of its tag.
#let kinds = (
  "追加": rgb("#1f7a5c"),
  "修正": rgb("#c8432b"),
  "変更": rgb("#2f62b5"),
  "削除": rgb("#6b4fa0"),
  "文書": rgb("#5d6772"),
)

#let sans = ("Noto Sans CJK JP",)
#let mono = ("JetBrains Mono", "Noto Sans Mono CJK JP")

// ------------------------------------------------------------------ state

#let deck-info = state("deck-info", (:))
// The item the following slides belong to: none before the first item and in
// the closing sections.
#let current-item = state("current-item", none)
#let item-counter = counter("item")
// What the strip above the slide title shows when no item is open.
#let section-name = state("section-name", none)

// ---------------------------------------------------------------- helpers

#let two-digits(n) = if n < 10 { "0" + str(n) } else { str(n) }

#let tag(body, fill: ink-soft) = box(
  fill: fill,
  radius: 2pt,
  inset: (x: 6pt, y: 0pt),
  outset: (y: 3.5pt),
  text(fill: white, weight: 700, size: 10pt, body),
)

#let kind-tag(kind) = {
  assert(kind in kinds, message: "unknown kind: " + kind + ". Use one of: " + kinds.keys().join(", "))
  tag(kind, fill: kinds.at(kind))
}

// A reference to an issue or a pull request: (label: "PR #12", url: "https://...").
// An entry without a url is shown as plain text (a commit that went in
// without a pull request, for example).
#let ref-links(refs) = {
  refs
    .map(r => if type(r) == dictionary and r.at("url", default: none) != none {
      link(r.url, r.label)
    } else if type(r) == dictionary {
      r.label
    } else {
      r
    })
    .join(h(0.9em))
}

// ------------------------------------------------------------------- deck

#let deck(
  // The name of the release (the tag).
  release: none,
  // The date the release was cut, as text.
  date: none,
  // What the deck covers, as text: "release-20261201-1..release-20261204-1".
  range: none,
  // The commit of develop the release took in (short hash).
  commit: none,
  // The pull request into main: (number: 12, url: "https://...").
  // none when there is none (a trial run).
  pr: none,
  // The repository, for the title slide: (name: "owner/repo", url: "https://...").
  repo: none,
  body,
) = {
  set document(title: "hizukuri リリースの報告 " + release, author: "hizukuri")
  set page(
    width: 960pt,
    height: 540pt,
    margin: (left: 44pt, right: 44pt, top: 34pt, bottom: 40pt),
    fill: paper,
    footer-descent: 12pt,
    footer: context {
      let info = deck-info.get()
      set text(size: 9pt, fill: ink-faint)
      grid(
        columns: (1fr, auto),
        [hizukuri リリースの報告#h(0.8em)#info.release],
        [
          #if info.pr != none [
            `main` への PR #link(info.pr.url)[\##info.pr.number]#h(1.2em)
          ]
          #counter(page).display() / #counter(page).final().first()
        ],
      )
    },
  )
  set text(font: sans, size: 14pt, fill: ink, lang: "ja", region: "JP", weight: 400)
  // Not justified: lines that mix Japanese with identifiers in boxes cannot
  // be stretched evenly, least of all in a narrow column.
  set par(leading: 0.78em, spacing: 1.05em, justify: false)
  set list(marker: text(fill: ember, sym.bullet), indent: 2pt, body-indent: 7pt, spacing: 0.85em)
  set enum(indent: 2pt, body-indent: 7pt, spacing: 0.85em, numbering: n => text(fill: ember, weight: 700, str(n) + "."))
  set strong(delta: 300)
  show emph: it => text(weight: 500, fill: ember, it.body)

  // Typst sets raw text at 0.8em; this brings it to 0.88em of the text
  // around it.
  set raw(theme: none)
  show raw: set text(font: mono, size: 1.1em)
  show raw.where(block: false): it => box(
    fill: wash,
    radius: 2pt,
    inset: (x: 2pt, y: 0pt),
    outset: (y: 3pt),
    it,
  )
  // A code block. A line that is wider than the block would wrap, which
  // makes a command hard to read and to copy: it is an error, like a slide
  // that does not fit.
  show raw.where(block: true): it => block(
    width: 100%,
    fill: night,
    radius: 3pt,
    inset: (x: 12pt, y: 10pt),
    {
      // 0.82em of the text around the block: 11.5pt in body text, and
      // smaller inside `small`.
      set text(fill: rgb("#e6e9ed"), size: 0.93em)
      set par(leading: 0.62em, justify: false)
      layout(size => {
        let wide = it.lines.filter(l => measure(l.body).width > size.width + 0.5pt)
        if wide.len() > 0 {
          let over = (code-line: wide.first().text, lines: wide.len())
          if sys.inputs.at("check", default: "false") == "true" {
            [#metadata(over) <overflow>]
          } else {
            panic("a line of a code block is too wide and would wrap: " + over.code-line)
          }
        }
        it
      })
    },
  )
  show link: it => if type(it.dest) == str {
    text(fill: rgb("#2f62b5"), underline(offset: 2.5pt, stroke: 0.5pt, it))
  } else {
    it
  }

  // Headings inside a slide.
  show heading: it => block(above: 1.1em, below: 0.7em, text(size: 12.5pt, weight: 700, fill: ember, it.body))

  set table(
    stroke: (x, y) => (bottom: 0.5pt + rule, top: if y == 0 { 1pt + ink } else { none }),
    inset: (x: 7pt, y: 6.5pt),
    fill: (x, y) => if y == 0 { wash },
  )
  show table: set text(size: 12pt)
  show table: set par(justify: false, leading: 0.6em)
  show table.cell.where(y: 0): set text(weight: 700)

  deck-info.update((release: release, date: date, range: range, commit: commit, pr: pr, repo: repo))
  body
}

// ----------------------------------------------------------------- slides

// Refuses to build a slide whose content does not fit, instead of letting it
// run onto a page of its own. With `--input check=true` (build.sh does this
// first), every such slide is recorded instead, so that all of them can be
// listed at once.
#let fit(name, body) = layout(size => {
  let needed = measure(block(width: size.width, body)).height
  if needed > size.height + 0.5pt {
    let over = (slide: name, needed: calc.round(needed.pt()), available: calc.round(size.height.pt()))
    if sys.inputs.at("check", default: "false") == "true" {
      [#metadata(over) <overflow>]
      block(height: size.height, clip: true, body)
    } else {
      panic(
        "the slide \"" + name + "\" does not fit: it needs " + str(over.needed) + "pt of "
          + str(over.available) + "pt. Split it into two slides or shorten it.",
      )
    }
  } else {
    body
  }
})

#let to-text(content) = {
  if type(content) == str { content } else if content.has("text") { content.text } else if content.has("children") {
    content.children.map(to-text).join()
  } else if content.has("body") { to-text(content.body) } else { " " }
}

// One slide. Belongs to the item that was opened last.
#let slide(title: none, body) = {
  pagebreak(weak: true)
  [#metadata("slide") <slide>]
  fit(if title == none { "(untitled)" } else { to-text(title) }, {
    context {
      let item = current-item.get()
      let section = section-name.get()
      set text(size: 10.5pt, fill: ink-soft)
      set par(justify: false)
      if item != none {
        grid(
          columns: (auto, 1fr, auto),
          column-gutter: 9pt,
          align: horizon,
          text(weight: 700, fill: ember, size: 12pt, two-digits(item.number)),
          [#kind-tag(item.kind)#h(8pt)#item.area#h(8pt)#text(fill: ink-faint)[｜]#h(8pt)#item.title],
          ref-links(item.refs),
        )
      } else if section != none {
        text(weight: 700, fill: ember, size: 12pt, section)
      } else {
        v(1em)
      }
    }
    v(-4pt)
    line(length: 100%, stroke: 0.5pt + rule)
    v(2pt)
    if title != none {
      block(below: 14pt, text(size: 23pt, weight: 700, title))
    }
    body
  })
}

// Opens an item: one unit of change (a pull request or an issue; several of
// them when they form one piece of work). The slides that follow belong to it.
//
//   kind   one of the keys of `kinds`
//   area   the part of the project that changed, as the reader would name it
//   title  what changed, in one phrase
//   refs   issues and pull requests: ((label: "Issue #8", url: "..."), (label: "PR #12", url: "..."))
#let item(kind: none, area: none, title: none, refs: ()) = {
  item-counter.step()
  context {
    let number = item-counter.get().first()
    current-item.update((number: number, kind: kind, area: area, title: title, refs: refs))
  }
  [#metadata((kind: kind, area: area, title: title, refs: refs)) <item>]
}

#let title-slide() = {
  pagebreak(weak: true)
  set page(fill: night, footer: none, margin: (left: 64pt, right: 64pt, top: 64pt, bottom: 48pt))
  context {
    let info = deck-info.get()
    set text(fill: white)
    set par(justify: false)
    rect(width: 46pt, height: 5pt, fill: ember)
    v(20pt)
    text(size: 15pt, fill: rgb("#aab3bd"), weight: 500)[hizukuri（火造り）]
    v(-2pt)
    text(size: 44pt, weight: 700)[リリースの報告]
    v(2pt)
    text(size: 24pt, weight: 500, font: mono, fill: rgb("#f0b9ac"), info.release)
    v(1fr)
    let items = query(<item>).len()
    let rows = (
      ([日付], info.date),
      ([範囲], raw(info.range)),
      ([取り込んだ `develop`], if info.commit != none { raw(info.commit) }),
      (
        [`main` への PR],
        if info.pr != none { link(info.pr.url, text(fill: rgb("#9cc0ff"))[\##info.pr.number]) } else { [なし（試作）] },
      ),
      ([項目の数], [#items]),
      ([リポジトリ], if info.repo != none { link(info.repo.url, text(fill: rgb("#9cc0ff"), info.repo.name)) }),
    ).filter(r => r.at(1) != none)
    {
      show raw.where(block: false): it => text(fill: white, it.text)
      set text(size: 13pt)
      grid(
        columns: (130pt, auto),
        row-gutter: 11pt,
        ..rows.map(r => (text(fill: rgb("#aab3bd"), r.at(0)), r.at(1))).flatten(),
      )
    }
  }
}

// The list of the items, with the page each one starts on. Comes after the
// slides that summarize the release and before the first item. `note` is
// shown under the list.
#let item-list(note: none) = slide(title: [項目の一覧], context {
  let items = query(<item>)
  set par(justify: false)
  let size = if items.len() > 14 { 10pt } else if items.len() > 10 { 10.8pt } else { 12pt }
  let pad = if items.len() > 14 { 3.9pt } else if items.len() > 10 { 4.6pt } else { 6pt }
  set text(size: size)
  show table: set text(size: size)
  table(
    columns: (auto, auto, auto, 1fr, auto, auto),
    inset: (x: 7pt, y: pad),
    align: (right, left, left, left, left, right),
    table.header([], [種類], [部分], [内容], [Issue、PR], [ページ]),
    ..items
      .enumerate()
      .map(((i, it)) => {
        let v = it.value
        let page = counter(page).at(it.location()).first() + 1
        (
          text(weight: 700, fill: ember, two-digits(i + 1)),
          kind-tag(v.kind),
          v.area,
          v.title,
          ref-links(v.refs),
          link(it.location(), str(page)),
        )
      })
      .flatten(),
  )
  if note != none {
    v(4pt)
    text(size: 10.5pt, fill: ink-soft, note)
  }
})

// Opens a section that is not an item: the summary at the start ("この報告の
// 内容"), how to check the release, the appendix. Closes the item that was
// open. The slides that follow show `name` in the strip.
#let section(name) = {
  current-item.update(none)
  section-name.update(name)
}

#let summary-section() = section("この報告の内容")
#let verify-section() = section("確かめ方")
#let appendix-section() = section("付録")

// ------------------------------------------------------------- components

// A pointer to another item: its number, as a link to its first slide.
// For the slide that shows how the items fit together.
#let see(number) = context {
  let items = query(<item>)
  assert(number >= 1 and number <= items.len(), message: "see(" + str(number) + "): there is no such item")
  link(items.at(number - 1).location(), box(
    fill: ember,
    radius: 2pt,
    inset: (x: 3.5pt, y: 0pt),
    outset: (y: 2.5pt),
    text(fill: white, weight: 700, size: 8.5pt, two-digits(number)),
  ))
}

// The first paragraph of an item: what changed, in two or three sentences.
#let lead(body) = block(
  width: 100%,
  below: 14pt,
  stroke: (left: 3pt + ember),
  inset: (left: 12pt, y: 3pt),
  text(size: 15.5pt, weight: 500, body),
)

// Two (or more) columns. `widths` are the column widths.
#let cols(widths: (1fr, 1fr), gutter: 26pt, ..cells) = grid(
  columns: widths,
  column-gutter: gutter,
  ..cells.pos()
)

// A screenshot or another picture, with a caption that says what the picture
// shows and how it was taken. `path` is relative to the deck's directory.
#let shot(path, caption: none, width: 100%, height: auto) = block(width: width, {
  let path = if path.starts-with("/") { path } else { "/" + path }
  block(
    stroke: 0.75pt + rule,
    radius: 2pt,
    clip: true,
    image(path, width: 100%, height: height),
  )
  if caption != none {
    v(-5pt)
    set par(justify: false, leading: 0.55em)
    text(size: 10pt, fill: ink-soft, caption)
  }
})

// A boxed remark: a limit, something not implemented, a thing to watch for.
#let note(label: "いまの制約", body) = block(
  width: 100%,
  fill: ember-wash,
  radius: 3pt,
  inset: (x: 11pt, y: 9pt),
  {
    set text(size: 12pt)
    set par(justify: false, leading: 0.62em)
    text(weight: 700, fill: ember, size: 10.5pt, label)
    v(-5pt)
    body
  },
)

// Smaller text, for a dense list or a long table.
#let small(body) = {
  set text(size: 12pt)
  set par(leading: 0.66em, spacing: 0.9em)
  set list(spacing: 0.72em)
  body
}

// Terms and what they mean, as two columns.
#let terms(width: auto, ..pairs) = {
  set par(justify: false, leading: 0.62em)
  grid(
    columns: (width, 1fr),
    column-gutter: 14pt,
    row-gutter: 9pt,
    ..pairs.pos().map(p => (text(weight: 700, size: 12.5pt, p.at(0)), text(size: 12.5pt, p.at(1)))).flatten(),
  )
}

// A command to run and what it should do. For the slides on how to check
// the release.
#let step(command, body) = block(width: 100%, below: 12pt, breakable: false, {
  raw(command, block: true, lang: "sh")
  v(-6pt)
  set text(size: 12pt)
  set par(justify: false, leading: 0.62em)
  body
})

// ---------------------------------------------------------------- diagram

// A box of a diagram.
#let node(body, fill: wash, stroke: 0.75pt + rule, width: 100%, height: auto, accent: false) = block(
  width: width,
  height: height,
  fill: if accent { ember-wash } else { fill },
  stroke: if accent { 0.75pt + ember } else { stroke },
  radius: 3pt,
  inset: (x: 9pt, y: 7pt),
  {
    set text(size: 11pt)
    set par(justify: false, leading: 0.55em)
    set align(center + horizon)
    body
  },
)

// Boxes from left to right with arrows between them.
#let flow(..nodes, gutter: 6pt) = {
  let cells = ()
  for (i, n) in nodes.pos().enumerate() {
    if i > 0 {
      cells.push(align(center + horizon, text(fill: ink-faint, size: 13pt, sym.arrow.r)))
    }
    cells.push(n)
  }
  grid(
    columns: range(cells.len()).map(i => if calc.even(i) { 1fr } else { auto }),
    column-gutter: gutter,
    align: horizon,
    ..cells,
  )
}

// Rows stacked on top of each other (a layer diagram). Each row is a list
// of boxes that share the width.
#let layers(..rows, gutter: 5pt) = stack(
  spacing: gutter,
  ..rows.pos().map(row => if type(row) == array {
    grid(columns: range(row.len()).map(_ => 1fr), column-gutter: gutter, ..row)
  } else {
    row
  }),
)
