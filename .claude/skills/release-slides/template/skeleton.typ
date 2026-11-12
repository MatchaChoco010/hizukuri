#import "lib/hizukuri-slides.typ": *

// 書き方は、skill `release-slides` の SKILL.md にある。
// 段落と箇条書きの 1 項目は、ソースの 1 行に書く（行の途中で折り返さない）。

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

#slide(title: [今回のリリースの要点])[
  #lead[
    今回のリリースで、何が変わったか。2〜3 文で書く。
  ]
  - *できるようになったこと。*利用者から見て、何が変わるか。
  - 項目が多いときは、全体の構成と、項目の対応を示すページを、この後に足す。
]

#item-list()

// ------------------------------------------------------------------- 01

#item(
  kind: "追加",
  area: "変わった部分",
  title: [何が、どうなったか],
  refs: (issue(0), pr(0)),
)

#slide(title: [このページで言いたいこと])[
  #lead[
    何が入ったか。何のためか。
  ]
  #cols(widths: (1fr, 1.1fr))[
    - 仕組みの説明。
    #note[
      いまの制約。
    ]
  ][
    #shot("img/NAME.png", caption: [
      何が写っているか。どう撮ったか。どこを見るか。
    ])
  ]
]

// -------------------------------------------------------------- 確かめ方

#verify-section()

#slide(title: [準備と、全部の検査])[
  #step("git fetch origin --tags\ngit switch --detach release-YYYYMMDD-N")[
    確かめる対象を取り出す。
  ]
  #step("nix develop --command tools/ci.sh")[
    何が起きれば、通っているか。
  ]
]

// ------------------------------------------------------------------ 付録

#appendix-section()

#slide(title: [PR を通らずに入った commit])[
  無ければ、このページを消す。
]
