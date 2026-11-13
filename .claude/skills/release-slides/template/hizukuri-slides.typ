// The template of the hizukuri release report: a deck of 16:9 slides that is
// read on a screen, one release per deck.
//
// A deck is one Typst file that imports this one:
//
//   #import "lib/hizukuri-slides.typ": *
//   #show: deck.with(release: "release-20270101-1", ...)
//   #title-slide()
//   #summary-section()
//   #topic[今回のリリースの要点]
//   #slide(caption: [...])[ ... ]
//   #item-list()
//   #chapter[...]                  (optional: groups the items that follow)
//   #item(kind: "追加", area: "検証用の compositor", title: [...], refs: (...))
//   #slide(caption: [...])[ ... ]
//   #slide(caption: [...])[ ... ]
//   #verify-section()
//   #topic[...]
//   #slide(caption: [...])[ ... ]
//
// A slide has no title of its own. Its heading is the title of the unit it
// belongs to (an item, or a topic in one of the other sections), so every
// slide of a unit carries the same heading and its position in the unit.
// What the one slide says goes into its caption, under the heading.
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

// Where a slide belongs is read from markers in the document, not kept in
// state: `<section>` opens a section, `<unit>` opens an item or a topic,
// `<item>` marks the units that are items, `<unit-slide>` marks a slide.
// A slide belongs to the last unit before it.

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

#let to-text(content) = {
  if type(content) == str { content } else if content.has("text") { content.text } else if content.has("children") {
    content.children.map(to-text).join()
  } else if content.has("body") { to-text(content.body) } else { " " }
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
    margin: (left: 60pt, right: 60pt, top: 32pt, bottom: 42pt),
    fill: paper,
    footer-descent: 14pt,
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
  set text(font: sans, size: 15pt, fill: ink, lang: "ja", region: "JP", weight: 400)
  // Not justified: lines that mix Japanese with identifiers in boxes cannot
  // be stretched evenly.
  set par(leading: 0.86em, spacing: 1.25em, justify: false)
  set list(marker: text(fill: ember, sym.bullet), indent: 2pt, body-indent: 8pt, spacing: 1.05em)
  set enum(indent: 2pt, body-indent: 8pt, spacing: 1.05em, numbering: n => text(fill: ember, weight: 700, str(n) + "."))
  set strong(delta: 300)
  show emph: it => text(weight: 500, fill: ember, it.body)

  // Typst sets raw text at 0.8em; this brings it to 0.88em of the text
  // around it.
  // Ligatures are off in code: `->` must stay two characters, as typed.
  set raw(theme: none)
  show raw: set text(font: mono, size: 1.1em, ligatures: false, features: (calt: 0))
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
    inset: (x: 14pt, y: 12pt),
    {
      // 0.82em of the text around the block.
      set text(fill: rgb("#e6e9ed"), size: 0.93em)
      set par(leading: 0.66em, justify: false)
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

  // Headings inside a slide: the title of a part of the slide.
  show heading: it => block(above: 1.3em, below: 0.8em, text(size: 13pt, weight: 700, fill: ember, it.body))

  set table(
    stroke: (x, y) => (bottom: 0.5pt + rule, top: if y == 0 { 1pt + ink } else { none }),
    inset: (x: 9pt, y: 8pt),
    fill: (x, y) => if y == 0 { wash },
  )
  show table: set text(size: 13pt)
  show table: set par(justify: false, leading: 0.62em)
  show table.cell.where(y: 0): set text(weight: 700)

  deck-info.update((release: release, date: date, range: range, commit: commit, pr: pr, repo: repo))
  body
}

// ------------------------------------------------------------------ units

// Opens a section that is not made of items: the summary at the start, how
// to try the release, the appendix. The topics that follow belong to it.
#let section(name) = [#metadata(name) <section>]

#let summary-section() = section("この報告の内容")
#let verify-section() = section("手元で動かして試す")
#let appendix-section() = section("付録")

// Opens a chapter: the items that follow, up to the next chapter or
// section, are about one part of the project. For a release whose items
// span many parts (the first release, for one), the items are grouped by
// chapter; inside a chapter they keep the order they went into develop.
// The name of the chapter is shown above the heading of its slides and as
// a row of the list of the items. Topics may open a chapter, to show what
// its items build together.
#let chapter(name) = [#metadata(name) <chapter>]

// The chapter in effect at a location: the last one before it, unless a
// section was opened after that chapter. none when there is none.
#let chapter-at(location) = {
  let chapters = query(selector(<chapter>).before(location))
  if chapters.len() == 0 { return none }
  let last = chapters.last()
  let later = query(selector(<section>).after(last.location()).before(location))
  if later.len() > 0 { none } else { last.value }
}

// Opens an item: one unit of change (a pull request or an issue; several of
// them when they form one piece of work). The slides that follow belong to
// it and carry its title as their heading.
//
//   kind   one of the keys of `kinds`
//   area   the part of the project that changed, as the reader would name it
//   title  the name of the change: a title, not a sentence about it
//   refs   issues and pull requests: ((label: "Issue #8", url: "..."), (label: "PR #12", url: "..."))
#let item(kind: none, area: none, title: none, refs: ()) = {
  assert(kind in kinds, message: "unknown kind: " + repr(kind) + ". Use one of: " + kinds.keys().join(", "))
  [#metadata((kind: kind, area: area, title: title, refs: refs)) <item>]
  [#metadata((title: title, item: (kind: kind, area: area, refs: refs))) <unit>]
}

// Opens a topic: a unit of slides in a section that is not made of items.
// The slides that follow carry its title as their heading.
#let topic(title) = [#metadata((title: title, item: none)) <unit>]

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

// One slide of the unit that was opened last.
//
//   caption  what this slide says, in one line. Shown under the heading.
#let slide(caption: none, body) = {
  pagebreak(weak: true)
  [#metadata(none) <unit-slide>]
  // Everything that is looked up in the document is looked up here, outside
  // of what `fit` measures.
  context {
    let units = query(selector(<unit>).before(here()))
    assert(units.len() > 0, message: "a slide needs an #item or a #topic before it")
    let unit = units.last()
    let following = query(selector(<unit>).after(here()))
    let of-unit = selector(<unit-slide>).after(unit.location())
    let total = if following.len() > 0 {
      query(of-unit.before(following.first().location())).len()
    } else {
      query(of-unit).len()
    }
    let position = query(of-unit.before(here())).len()
    let number = query(selector(<item>).before(here())).len()
    let sections = query(selector(<section>).before(here()))
    let chapter = chapter-at(here())
    let unit = unit.value
    let whole = {
      set par(justify: false)
      // Above the heading: where the unit belongs.
      {
        set text(size: 10.5pt, fill: ink-soft)
        if unit.item != none {
          grid(
            columns: (1fr, auto),
            align: horizon,
            [#if chapter != none [#text(weight: 700, fill: ember, chapter)#h(10pt)]#kind-tag(unit.item.kind)#h(8pt)#unit.item.area],
            ref-links(unit.item.refs),
          )
        } else if chapter != none {
          text(weight: 700, fill: ember, chapter)
        } else if sections.len() > 0 {
          text(weight: 700, fill: ember, sections.last().value)
        } else {
          v(1em)
        }
      }
      v(-2pt)
      // The heading: the title of the unit, the same on all of its slides,
      // and which of its slides this is.
      grid(
        columns: (1fr, auto),
        column-gutter: 12pt,
        align: (left + bottom, right + bottom),
        text(size: 23pt, weight: 700, {
          if unit.item != none {
            text(fill: ember, two-digits(number))
            h(12pt)
          }
          unit.title
        }),
        text(size: 12pt, fill: ink-faint, weight: 500)[#position / #total],
      )
      if caption != none {
        v(-6pt)
        text(size: 14.5pt, fill: ink-soft, weight: 500, caption)
      }
      v(-3pt)
      line(length: 100%, stroke: 0.5pt + rule)
      v(8pt)
      body
    }
    fit(if caption == none { "(no caption)" } else { to-text(caption) }, whole)
  }
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

// The list of the items, with the pages each one covers. Comes after the
// slides that summarize the release and before the first item. `pages` is
// the number of slides the list is spread over (seven items fit on one), or
// the number of items on each of them: (6, 7, 5). `note` is shown under the
// end of the list. When the items are grouped in chapters, each chapter
// starts with a row that names it.
#let item-list(pages: 1, note: none) = {
  topic[項目の一覧]
  let counts = if type(pages) == array { pages } else { none }
  let slides = if counts != none { counts.len() } else { pages }
  let caption = context if query(<chapter>).len() > 0 {
    [章ごとに並べた。章の中の順は、`develop` に入った順である]
  } else {
    [番号の順は、`develop` に入った順である]
  }
  for index in range(slides) {
    slide(caption: caption, context {
      let items = query(<item>)
      let (first, last) = if counts != none {
        let first = counts.slice(0, index).sum(default: 0)
        (calc.min(items.len(), first), calc.min(items.len(), first + counts.at(index)))
      } else {
        let per-slide = calc.max(1, calc.ceil(items.len() / slides))
        let first = calc.min(items.len(), index * per-slide)
        (first, calc.min(items.len(), first + per-slide))
      }
      if counts != none and index == slides - 1 {
        assert(
          last == items.len(),
          message: "item-list: the counts in `pages` cover " + str(last) + " of the " + str(items.len()) + " items",
        )
      }
      let page-of = location => counter(page).at(location).first()
      let chapter-of = i => chapter-at(items.at(i).location())
      set par(justify: false)
      show table: set text(size: 12.5pt)
      table(
        columns: (auto, auto, 1fr, auto, auto),
        inset: (x: 9pt, y: 6pt),
        align: (right + horizon, left + horizon, left, left + horizon, right + horizon),
        table.header([], [種類], [題と、変わった部分], [Issue、PR], [ページ]),
        ..range(first, last)
          .map(i => {
            let it = items.at(i)
            let v = it.value
            // The slides of the item: from its marker to the next unit. The
            // first unit after the marker is the item's own.
            let own = selector(<unit-slide>).after(it.location())
            let units = query(selector(<unit>).after(it.location()))
            let slides = if units.len() > 1 { query(own.before(units.at(1).location())) } else { query(own) }
            let start = if slides.len() > 0 { page-of(slides.first().location()) } else { page-of(it.location()) + 1 }
            let end = if slides.len() > 0 { page-of(slides.last().location()) } else { start }
            let chapter = chapter-of(i)
            let opens = chapter != none and (i == first or chapter-of(i - 1) != chapter)
            let heading = if opens {
              (
                table.cell(colspan: 5, fill: wash, align: left + horizon, inset: (x: 9pt, y: 5pt), text(
                  size: 11.5pt,
                  weight: 700,
                  fill: ember,
                  chapter,
                )),
              )
            } else { () }
            let row = (
              text(weight: 700, fill: ember, two-digits(i + 1)),
              kind-tag(v.kind),
              [#text(weight: 500, v.title) \ #text(size: 10.5pt, fill: ink-soft, v.area)],
              ref-links(v.refs),
              link(it.location(), if end > start [#start〜#end] else [#start]),
            )
            heading + row
          })
          .flatten(),
      )
      if note != none and index == slides - 1 {
        v(4pt)
        text(size: 11.5pt, fill: ink-soft, note)
      }
    })
  }
}

// ------------------------------------------------------------- components

// A pointer to another item: its number, as a link to its first slide.
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

// The opening paragraph of an item: what changed and what for, in two or
// three sentences.
#let lead(body) = block(
  width: 100%,
  below: 20pt,
  stroke: (left: 3pt + ember),
  inset: (left: 14pt, y: 4pt),
  text(size: 16.5pt, weight: 500, body),
)

// Two columns, for a picture next to the text that explains it. Text alone
// is not set in columns.
#let cols(widths: (1fr, 1fr), gutter: 32pt, ..cells) = grid(
  columns: widths,
  column-gutter: gutter,
  ..cells.pos()
)

// A screenshot or another picture, with a caption that says what the picture
// shows and how it was taken. `path` is relative to the deck's directory.
// With a `height`, the picture is centered at that height: for a slide that
// holds a picture and its caption and nothing else.
#let shot(path, caption: none, width: 100%, height: auto) = {
  let path = if path.starts-with("/") { path } else { "/" + path }
  let picture = block(
    stroke: 0.75pt + rule,
    radius: 2pt,
    clip: true,
    if height == auto { image(path, width: 100%) } else { image(path, height: height) },
  )
  let note = if caption != none {
    set par(justify: false, leading: 0.62em)
    text(size: 11.5pt, fill: ink-soft, caption)
  }
  if height == auto {
    block(width: width, {
      picture
      if caption != none { v(-4pt); note }
    })
  } else {
    align(center, picture)
    if caption != none { v(-2pt); note }
  }
}

// A boxed remark: a limit, something not implemented, a thing to watch for.
#let note(label: "いまの制約", body) = block(
  width: 100%,
  fill: ember-wash,
  radius: 3pt,
  inset: (x: 14pt, y: 11pt),
  {
    set text(size: 13.5pt)
    set par(justify: false, leading: 0.7em)
    text(weight: 700, fill: ember, size: 11pt, label)
    v(-5pt)
    body
  },
)

// Terms and what they mean.
#let terms(width: auto, ..pairs) = {
  set par(justify: false, leading: 0.7em)
  grid(
    columns: (width, 1fr),
    column-gutter: 18pt,
    row-gutter: 14pt,
    ..pairs.pos().map(p => (text(weight: 700, p.at(0)), p.at(1))).flatten(),
  )
}

// A command to run and what should happen. For the slides on how to try
// the release.
#let step(command, body) = block(width: 100%, below: 16pt, breakable: false, {
  raw(command, block: true, lang: "sh")
  v(-4pt)
  set text(size: 13.5pt)
  set par(justify: false, leading: 0.7em)
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
  inset: (x: 10pt, y: 10pt),
  {
    set text(size: 12.5pt)
    set par(justify: false, leading: 0.6em)
    set align(center + horizon)
    body
  },
)

// Boxes from left to right with arrows between them.
#let flow(..nodes, gutter: 8pt) = {
  let cells = ()
  for (i, n) in nodes.pos().enumerate() {
    if i > 0 {
      cells.push(align(center + horizon, text(fill: ink-faint, size: 14pt, sym.arrow.r)))
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
#let layers(..rows, gutter: 6pt) = stack(
  spacing: gutter,
  ..rows.pos().map(row => if type(row) == array {
    grid(columns: range(row.len()).map(_ => 1fr), column-gutter: gutter, ..row)
  } else {
    row
  }),
)
