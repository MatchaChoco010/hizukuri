#import "lib/hizukuri-slides.typ": *

// 書き方は、skill `release-slides` の SKILL.md にある。
// - 見出しは、題名である。#item と #topic の題に、文を書かない。そのページで言うことは、#slide の caption に書く。
// - 段落と箇条書きの 1 項目は、ソースの 1 行に書く（行の途中で折り返さない）。
// - 詰め込まない。段組み（#cols）は、画面や図の横に、その説明を置くときだけ使う。

#let repo-url = "https://github.com/OWNER/REPO"
#let pr(number) = (label: "PR #" + str(number), url: repo-url + "/pull/" + str(number))
#let issue(number) = (label: "Issue #" + str(number), url: repo-url + "/issues/" + str(number))
#let commit(hash) = (label: hash, url: repo-url + "/commit/" + hash)

#show: deck.with(
  release: "release-YYYYMMDD-N",
  date: "YYYY-MM-DD",
  range: "release-YYYYMMDD-N..release-YYYYMMDD-N",
  commit: "0000000",
  pr: (number: 0, url: repo-url + "/pull/0"),
  repo: (name: "OWNER/REPO", url: repo-url),
)

#title-slide()

// ------------------------------------------------------------ この報告の内容

#summary-section()

#topic[今回のリリースの要点]

#slide(caption: [このリリースで、何が変わったか])[
  #lead[
    今回のリリースで、何が入ったか。2〜3 文で書く。
  ]
  - *できるようになったこと。*利用者から見て、何が変わるか。
]

#slide(caption: [まだ無いものと、気を付けること])[
  - まだ無いもの。
]

// 項目が多いときは、「全体の構成と、項目の対応」の topic を、ここに足す。

#item-list()

// ------------------------------------------------------------------- 01

#item(
  kind: "追加",
  area: "変わった部分",
  title: [変更の名前（題名。文にしない）],
  refs: (issue(0), pr(0)),
)

#slide(caption: [何が入ったか、1 行で])[
  #lead[
    何が入ったか。何のためか。
  ]
  - 仕組みの説明。
]

#slide(caption: [この画面に、何が写っているか])[
  #shot("img/NAME.png", height: 318pt, caption: [
    どう取り込んだか。どこを見るか。
  ])
]

#slide(caption: [いまの制約])[
  - 残っていること。
  #note[
    未実装のもの。
  ]
]

// ------------------------------------------------------ 手元で動かして試す

#verify-section()

#topic[いま、手元で試せること]

#slide(caption: [何が、どの形で動かせて、何が、まだ動かせないか])[
  #table(
    columns: (auto, auto, 1fr),
    table.header[動かし方][状態][内容],
    [検証用の VM の中で], [試せる], [起動して、キーボードとマウスで触れる],
    [日常のデスクトップとして], [まだ使えない], [足りないもの],
  )
]

#topic[動かし方の名前]

#slide(caption: [起動の手順])[
  #step("起動のコマンド")[
    何が起きるか。
  ]
  + 次に、何をするか。
]

#slide(caption: [触って、見るところ])[
  #table(
    columns: (auto, 1fr),
    table.header[操作][起きること],
    [キー], [こうなる],
  )
]

// ------------------------------------------------------------------ 付録

#appendix-section()

#topic[PR を通らずに入った commit]

#slide(caption: [無ければ、この topic を消す])[
  - commit と、その内容、属する項目。
]
