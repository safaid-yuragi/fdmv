# macOS での動作確認手順

開発環境（Linux）では macOS 向けのコンパイルチェック（`cargo check --target aarch64-apple-darwin` / `x86_64-apple-darwin`）までしか確認できていません。
この手順で、実機でのビルドと動作を確認してください。所要時間は 20〜30 分程度です。

## 1. 準備

```sh
# Xcode のコマンドラインツール（未導入の場合）
xcode-select --install

# Homebrew（未導入の場合は https://brew.sh/ の手順で）
brew install dav1d opus pkg-config ffmpeg

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## 2. ビルドとテスト

プロジェクトのフォルダで:

```sh
cargo test --workspace
cargo build --release
```

- テストはすべて `ok` になるはずです（ffmpeg を使うテストも含む）。
- 失敗した場合は、出力全体を保存して送ってください。

## 3. テスト用ファイルを作る

```sh
mkdir -p /tmp/fdmv && cd /tmp/fdmv
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i sine=f=440:r=48000 \
       -t 20 -c:v libx264 -c:a aac -shortest input.mp4
ffmpeg -f lavfi -i "sine=f=880:r=48000" -t 3 voice.wav

FDMV=<プロジェクトのフォルダ>/target/release
$FDMV/fdmv pack input.mp4 -o test.fdmv --chain "解説=voice.wav@5" --chain "解説=voice.wav@12" --gain 解説=-3
$FDMV/fdmv verify test.fdmv          # 最後に "result: OK" と出ること
```

## 4. 自己テスト（自動）

```sh
FDMV_DEBUG_SCREENSHOT=/tmp/fdmv/shot.ppm $FDMV/fdmv-player /tmp/fdmv/test.fdmv
```

ウィンドウが開き、約 2 秒後に自動で閉じます。端末に次のような行が出ます。

```
debug: wall=1.5xxs position=3.5xxs frame_pts=Some(3.5xx) playing=true audio=Some("…") save=Ok(())
```

- `position` が 3.5 秒前後（2 秒＋再生した時間）であること
- `frame_pts` と `position` の差が 0.05 秒未満であること
- `audio` が `Some(...)` であること（出力デバイス名）

`shot.ppm`（または `sips -s format png shot.ppm --out shot.png` で変換したもの）も送ってください。

## 5. 手動で確認すること

`$FDMV/fdmv-player /tmp/fdmv/test.fdmv` で起動して確認します。

| # | 確認内容 | 期待する結果 |
|---|---|---|
| 1 | 画面の文字 | メニューやチェーン名（「解説」）が日本語で表示される（□ にならない） |
| 2 | Space キー | 再生／一時停止。440 Hz の音が鳴る |
| 3 | 右パネルで「解説」に ✓ | 5〜8 秒と 12〜15 秒で 880 Hz の音が重なって聞こえる |
| 4 | シークバーの上のオレンジの帯 | 5〜8 秒と 12〜15 秒の位置に表示される |
| 5 | シークバーをクリック／ドラッグ | その位置に移動し、映像と音がずれない（左上のタイムコードと再生位置が一致） |
| 6 | 「main」の ✓ を外す | 440 Hz の音が止まる（解説だけ鳴る） |
| 7 | 音量スライダー | チェーンごと・全体の音量が変わる |
| 8 | ⌘O | ファイル選択ダイアログが開き、.fdmv を開ける |
| 9 | Finder から .fdmv をウィンドウにドラッグ | そのファイルが開く |
| 10 | F キー／映像をダブルクリック | 全画面になり、Esc で戻る |
| 11 | 最後まで再生 | 自動で一時停止し、もう一度 Space で先頭から再生 |
| 12 | Retina ディスプレイ | 文字や映像がぼやけない |

## 6. エディタの確認

```sh
$FDMV/fdmv-editor
```

| # | 確認内容 | 期待する結果 |
|---|---|---|
| 1 | `input.mp4` と `voice.wav` をウィンドウにドロップ | 素材一覧に出て、「プロキシ作成中」の後に消える。input.mp4 は映像トラックに並ぶ |
| 2 | Space | プレビューが再生される |
| 3 | 再生位置を 5 秒にして S | 映像クリップが 2 つに分かれる |
| 4 | 後半のクリップをクリックして Delete | 削除され、後ろが詰まる。⌘Z で元に戻る |
| 5 | 「＋ チェーンを追加」→ voice.wav をその行にドラッグ | 音声クリップが置かれ、再生すると 880 Hz が重なる |
| 6 | I / O で範囲を決めて Shift+Delete | すべてのトラックから範囲が消えて詰まる |
| 7 | ⌘S で保存 → ⌘O で開き直す | 同じ状態に戻る |
| 8 | ⌘E → 書き出し開始 | 進捗が出て完了。`fdmv verify` で "result: OK" |

## 7. 録画アプリの確認

```sh
# 自己テスト（テストパターンを 4 秒録画して保存し、自動で閉じる）
FDMV_TEST_PATTERN=1 FDMV_DEBUG_SCREENSHOT=/tmp/fdmv/rec.ppm $FDMV/fdmv-recorder
```

- 初回は「画面収録」の許可を求められます。**システム設定 → プライバシーとセキュリティ → 画面収録とシステムオーディオ録音**で、
  起動に使った端末アプリ（ターミナル / iTerm など）を許可し、端末を起動し直してからもう一度実行してください。
- 何かアプリ（Safari など）のウィンドウが開いている状態で実行してください（録音するアプリが 1 つ以上必要です）。
- 端末に `debug: saved /tmp/fdmv/録画 ….fdmv (… bytes)` と出れば成功です。`rec.ppm` も送ってください。

続いて手動で確認します（`$FDMV/fdmv-recorder`）。Safari で音の出る動画（YouTube など）、「ミュージック」で曲を再生しておきます。

| # | 確認内容 | 期待する結果 |
|---|---|---|
| 1 | 左上の「映像」の一覧 | ディスプレイと、開いているウィンドウが並ぶ |
| 2 | ディスプレイを選ぶ | 下にプレビューが出る（Retina でも文字がつぶれない大きさ） |
| 3 | 右の「音声」の一覧 | Safari・ミュージックなど、ウィンドウのあるアプリが並ぶ |
| 4 | Safari に ✓・メイン、ミュージックに ✓、「マイクも録音する」に ✓ | 録画開始ボタンが押せるようになる（マイクの許可を求められたら許可） |
| 5 | 録画開始 → 20 秒ほど待つ | 右下に各チェーンのレベルメーターが動く（Safari・ミュージック・マイクが別々に） |
| 6 | 録画中にほかのアプリに ✓ | そのアプリのチェーンが途中から追加される |
| 7 | 停止して保存 | 進捗の後「保存しました」と出る。「再生」でプレイヤーが開く |
| 8 | プレイヤーで再生 | 既定では Safari の音だけが鳴る。チェーンの ✓ でミュージック・マイクを重ねられる |
| 9 | 映像と音のずれ | 動画の口の動きと音が合っている |
| 10 | 映像に「ウィンドウ: …」を選んで録画 | そのウィンドウだけが録画される |
| 11 | `$FDMV/fdmv verify "<保存したファイル>"` | "result: OK" |
| 12 | 下の「エンコーダ」欄 | 「自動: GPU（Apple HEVC）→ 停止後に AV1 へ変換」になる。録画中も重くならず、停止後に変換の進捗が出る |

## 8. 既知の制限

- Finder で .fdmv をダブルクリックして開く（「このアプリケーションで開く」）には未対応です。端末から起動するか、ウィンドウにドラッグしてください。
- `.app` バンドルにはしていません（端末から実行ファイルを直接起動します）。
- 録画アプリのアプリ単位の音声は macOS 13 以降が必要です。macOS 13 では Retina の画面が実際の画素数ではなく、ポイント単位の大きさで録画されます（14 以降は画素数どおり）。

## 9. 報告してほしい情報

- macOS のバージョンと CPU（Apple Silicon / Intel）: `sw_vers; uname -m`
- 手順 2〜7 の結果（うまくいかなかった項目は端末の出力も）
