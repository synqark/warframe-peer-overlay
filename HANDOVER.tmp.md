# 引き継ぎメモ（一時ファイル・コミットしない）

`CLAUDE.md` に書いていない最新の状況と前提知識だけをまとめたもの。設計そのものは `CLAUDE.md` の Architecture を参照。

## 現在の状態

- main の作業ツリーに未コミットの変更がある: ロードアウト保存機能（分隊メンバー＋自分）
  - `src/loadout.rs`（新規）, `src/parser.rs`, `src/monitor.rs`, `src/lib.rs`, `Cargo.toml`, `Cargo.lock`, `README.md`, `CLAUDE.md`
  - 手元で `cargo fmt --all --check` / `cargo clippy --all-targets`（`RUSTFLAGS=-D warnings`）/ `cargo test` / `cargo build --release` は通過済み
- **未実施: 新ビルドを実際のゲームで動かす確認**
  - 別の場所に置いた旧 overlay が起動していると、二重起動防止で新ビルドは起動しない。タスクトレイから Exit してから `target\release\warframe-peer-overlay.exe` を起動する
  - 確認すること（保存先は `%LOCALAPPDATA%\synqark\WarframePeerOverlay\data\loadouts\`）
    - 起動直後に `self_latest.json` ができる
    - アーセナルで装備を変えると、数秒で `self\<ミリ秒>.json` が1つ増え、`self_latest.json` が更新される（開いただけでは増えない）
    - 分隊に誰かが参加すると `<unix時刻>_<名前>_<プラットフォーム>.json` ができる
  - ゲーム内の操作（アーセナル、セッション参加・ホスト）はユーザーに頼めば協力してもらえる
- 次の作業: 実地確認のあと、ブランチを切ってコミット（ユーザーの指示待ち）

## ユーザーと決めた方針

- メモリの読み取りは**常時有効**（オプトインにしない）。README の免責事項・プライバシーはこの前提に書き換え済み
- 取得したロードアウトは **JSON 保存のみ**。overlay の画面には何も追加しない（メンバー・自分とも）
- アカウントIDは取得しない（理由は下記）
- 軽量化: 分隊メンバーの管理は既存の `LogParser` を使い、独自の状態管理（参加中・離脱の区別）やギア一覧による予備の照合は入れていない

## 前提知識（調査済みなので再調査は不要）

- ロードアウト JSON の主なキー（1人30〜40 KB）
  - `PlayerLevel`（MR）, `PlayerXp`, `KubrowName`, `Consumables`（ギア一覧）, `FocusAbility`, `AuraName`
  - `NORMAL`: 観測上の並びは `[0]`フレーム `[1]`セカンダリ `[2]`プライマリ `[3]`近接 `[4]`アークガン `[5]`アルティメット武器。各要素に `ItemType`, `Level`, `Polarized`（フォーマ数）, `WeaponUpgrades`（スキンと装着MODのパス）
  - ほかに `SENTINEL`, `ARCHWING`, `OPERATOR`, `MECH`, `KDRIVE`, `DATAKNIFE`, `CrewShipLoadOut` など
- 自分のロードアウト
  - アーセナルで装備を変えるたびに、`BuildLoadOut` のログと同じ秒に新しい版がメモリにできる。閉じたとき（`OnSaveLoadOutCompleteCommon` → `SendLoadOut`）は中身が同じなら新版はできない
  - 置き換わった版は数分で消えるが、別プリセットとみられる古い版がコピー20個前後で残ることがある。起動直後の「コピー最多を採用」がこれに負ける可能性はゼロではない（最初の装備変更で正しい版に切り替わる）
  - 自分の名前は `Logged in <名前>` で分かる（最初の `AddSquadMember` より前に出る）。自分の JOIN 行は `from , loadout: 0 bytes` として出る
- `mm=`（`AddSquadMember` / `AddPlayerToSession` / `RemovePlayerFromSession`）の形式はプラットフォームで違う
  - U+E000（PC）: 24桁hex。ビット反転するとアカウントIDになる
  - U+E001: 16桁の10進数。U+E002: 表示名そのもの
  - そのため全員分のアカウントIDはログから得られない。ホストのとき離脱時の `SendSessionUpdate` に `"memberAccountId"` が出るが、確認できたのは PC の相手だけ
- メモリ上に「名前・JSON・アカウントID」を並べた分隊テーブルがあるが、作られるのはセッション終了時（ミッション完了・離脱）だけ。リアルタイムには使えない
- リレー（ハブ）のプレイヤーは別経路で、`/Temp/HubPlayers/<アカウントID>` 向けに `{"a":{"a":"…"}}` 形式の外見だけの短縮 JSON（MODなし）が届く。overlay では扱っていない
- JOIN 行はクイックマッチ待機中に一瞬だけ入った分隊でも出るので、実際には一緒に遊ばなかった人のロードアウトも保存される（許容済み）
- ミッション中は EE.log の書き出しが5秒ほど遅れることがある。JSON 自体は JOIN とほぼ同時にメモリに現れている

## 関連リソース

- 調査用ツール `F:\wf-vendor-probe`（Rust CLI、git 管理外）。`squad` サブコマンドに同じ取得方法を実装済みで、こちらには参加中・離脱の状態管理、ギア一覧による予備照合、`--replay`（ログ再生）もある。README に実測メモあり
