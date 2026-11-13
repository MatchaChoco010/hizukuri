# hizukuri

hizukuri（火造り）は、Wayland compositor を作るための基盤です。ゲームエンジンがゲームを作るための基盤であるのと、同じ関係にあります。
リアルタイムグラフィクスとスクリプティングで、Wayland compositor の動作を作ります。

- コアに、完成された機能を組み込みません。レンダラー、シーンの表現、毎フレームの処理、ウィンドウの管理を、スクリプトの層で書きます。
- カラーマネジメントを、最初から組み込みます。
- GPU は Vulkan 1.4 を使います。バインディングは [vulkan-rs](https://github.com/MatchaChoco010/vulkan-rs) です。

デザインは [docs/design/direction.md](docs/design/direction.md) にあります。

## いまの状態

開発の初期の段階です。いまあるコードは、基盤の仕組みと、DRM/KMS の session が通ることを確かめるための、検証用の compositor です。
製品のコードではありません。作りと動かし方は [docs/prototype/README.md](docs/prototype/README.md) にあります。

## ビルドと検査

[Nix](https://nixos.org/) の flake で、開発環境を固定しています。

```sh
nix develop --command tools/ci.sh
```

`tools/ci.sh` は、format の確認、lint、テストを行います。テストは、画面も GPU も使いません（CPU で描く Vulkan の実装、lavapipe で動きます）。
Nix を使わない場合に要るものは、[docs/dev/development-environment.md](docs/dev/development-environment.md) にあります。

CI に [RunsOn](https://runs-on.com) を利用しています。

## 文書

| 文書 | 内容 |
| --- | --- |
| [docs/design/](docs/design/) | 何を作るか |
| [docs/prototype/](docs/prototype/) | 検証用の compositor の作りと動かし方 |
| [docs/dev/](docs/dev/) | 開発環境、テスト、CI、検証用の VM |
| [docs/research/wayland/](docs/research/wayland/) | 実装に要る Wayland のプロトコルのまとめ |
| [AGENTS.md](AGENTS.md) | このリポジトリで作業するときの決まり |

## 謝辞

hizukuri は、先行する Wayland compositor と、ライブラリに、多くを負っています。

- [Smithay](https://github.com/Smithay/smithay): Wayland のプロトコルの処理と、seat、session、udev、libinput の層に、ライブラリとして使っています。
  API の使い方は、Smithay の example の compositor（anvil、smallvil）に倣いました。
- [niri](https://github.com/niri-wm/niri): 標準で用意するコンポジターの手本にしている、スクロール型タイルの compositor です。
  ジェスチャー、アニメーションの時刻、xwayland-satellite の起動などの振る舞いを調べ、参考にしました。
- [ShojiWM](https://github.com/bea4dev/ShojiWM): スクリプトで compositor を作る、先行するプロジェクトです。
  Smithay の上にカラーマネジメントとティアリングを足す構成と、direct scanout とティアリングについての文書を、参考にしました。
- [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite): X11 のアプリケーションを、これに任せています。
- sway、wlroots、KWin、Mutter、Hyprland、cosmic-comp、Weston: プロトコルの扱いを決めるときに、それぞれの振る舞いを調べて、比べました。

第三者のソフトウェアについての表示は、[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) にあります。

## ライセンス

次の、どちらかを選んで、使えます。

- Apache License, Version 2.0（[LICENSE-APACHE](LICENSE-APACHE)）
- MIT license（[LICENSE-MIT](LICENSE-MIT)）

hizukuri に取り込むために送られた貢献は、明示的な断りが無いかぎり、Apache-2.0 の定義に従い、上の 2 つの license の両方で提供されるものとします。
追加の条件は、付きません。
