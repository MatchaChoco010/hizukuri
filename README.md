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

license は、まだ決まっていません。

## ビルドと検査

[Nix](https://nixos.org/) の flake で、開発環境を固定しています。

```sh
nix develop --command tools/ci.sh
```

`tools/ci.sh` は、format の確認、lint、テストを行います。テストは、画面も GPU も使いません（CPU で描く Vulkan の実装、lavapipe で動きます）。
Nix を使わない場合に要るものは、[docs/dev/development-environment.md](docs/dev/development-environment.md) にあります。

## 文書

| 文書 | 内容 |
| --- | --- |
| [docs/design/](docs/design/) | 何を作るか |
| [docs/prototype/](docs/prototype/) | 検証用の compositor の作りと動かし方 |
| [docs/dev/](docs/dev/) | 開発環境、テスト、CI、検証用の VM |
| [docs/research/wayland/](docs/research/wayland/) | 実装に要る Wayland のプロトコルのまとめ |
| [AGENTS.md](AGENTS.md) | このリポジトリで作業するときの決まり |
