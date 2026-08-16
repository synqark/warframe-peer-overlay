# Warframe Peer Overlay

Windows上でWarframeの分隊ピア情報を表示するオーバーレイです。

## 免責事項

**本ソフトウェアは無保証で提供されます。本ソフトウェアの使用または使用不能によって生じたいかなる損害・不利益・トラブルについても、作者は一切の責任を負いません。すべて自己責任でご利用ください。**

本プロジェクトは非公式であり、Digital Extremes Ltd. および Warframe とは一切関係がありません。同社から提携・承認・後援・支持を受けたものではありません。Warframe は Digital Extremes Ltd. の商標または登録商標です。

本ソフトウェアはゲームへの介入を行いません。Warframeが自ら出力するログファイル (`EE.log`) の読み取りと、オーバーレイ位置合わせのためのウィンドウ位置の取得のみを行い、ゲームクライアントの改変、メモリの読み書き、プロセスへのインジェクション、通信の傍受はいずれも行いません。ただし、利用がゲームの利用規約に抵触しないかについてはご自身でご判断ください。

## 機能

- `Warframe.x64.exe` / `Warframe.exe` の起動検出
- `%LOCALAPPDATA%\Warframe\EE.log` の追尾
- 分隊メンバー、接続先IP、ホスト候補の表示
- 国・地域・ASN表示と、リレー/VPNの可能性があるホスティングASNの表示

[![スクリーンショット](assets/readme_overlay.png)](assets/readme_overlay.png)

## 使い方

[Releases](https://github.com/synqark/warframe-peer-overlay/releases) から `warframe-peer-overlay.exe` をダウンロードして実行してください。インストールは不要です。

オーバーレイは常にゲーム入力を透過し、タスクバーには表示されません。releaseビルドではコマンドプロンプトも表示されません。終了するには、Windowsのタスクトレイにあるアイコンを右クリックし、`Exit` を選択してください。


[![スクリーンショット](assets/readme_tray.png)](assets/readme_tray.png)


すでに起動している状態でもう一度exeを実行した場合、二重に起動せず、その旨をWindows通知で知らせて終了します。

## プライバシー

ログ解析とホスト判定はローカルで実行します。地域情報が有効な場合、検出した公開IPを `https://ipinfo.io` に問い合わせます。結果はユーザーのキャッシュディレクトリへ30日間保存します。EE.log、分隊名、IPアドレスのログ出力やアップロードは行いません。

外部問い合わせを行わない場合:

```powershell
warframe-peer-overlay.exe --no-geo
```

## ビルド

WindowsにRust 1.88以降をインストールし、PowerShellで実行します。

```powershell
cargo build --release
```

生成物: `target\release\warframe-peer-overlay.exe`

## 制約

- Windows専用です。
- EE.logの形式はWarframe更新で変わる可能性があります。
- `HOST` はEE.logの `Squad host address` とピアIPが一致した場合のみ表示します。
- `Relay/VPN?` はASN名による推定であり、VPN利用を断定するものではありません。
- Windows通知は、インストーラを持たないexeのためアプリ登録がなく、`Windows PowerShell` の名前とアイコンで表示されます。

## ライセンス

[The Unlicense](LICENSE) — パブリックドメインに献呈しています。著作権表示なしで、商用・非商用を問わず自由に利用、改変、再配布できます。
