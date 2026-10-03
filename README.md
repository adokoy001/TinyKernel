# Tane OS — Rustで作る最小の自前カーネル

作成日: 2026 10 02

既存OSの上で動くシェルではなく、仮想PCのCPUを直接動かす小さなカーネルです。
OS本体、BIOS用ブートセクター、画面・入力ドライバを、このプロジェクト内で実装しています。
Linuxのコード、GRUB、既成のRustブートローダー、外部crate、libcは使用しません。
Rustが提供する`core`とコンパイラの組み込み処理は使用します。

「自前」の範囲は、ブートセクター以降のOSのコードです。PCのBIOSファームウェア、Rustコンパイラ、GNU binutils、QEMUは開発・実行基盤として使います。
Rustは16ビットのBIOS入口を通常のターゲットとして扱えないため、この短い入口はアセンブリです。カーネル本体はRustです。

## まず、同梱イメージを起動する

対象環境はx86_64 Linux、またはWindows上のUbuntu / WSLです。
PC BIOSを持つQEMU仮想PCとして起動します。UEFIや実機USB起動には対応していません。

UbuntuでQEMUを用意します。

```sh
sudo apt update
sudo apt install qemu-system-x86
```

ZIPを展開して、このREADMEと同じフォルダで実行します。起動イメージは同梱済みなので、Rustをインストールしなくても試せます。

```sh
sh run.sh
```

画面をクリックして、`help`と入力してください。入力は英語配列・ASCIIです。マウスとキーボードのキャプチャは`Ctrl+Alt+G`で解除できます。

画面を開けない環境では、端末に接続するモードが使えます。

```sh
sh run.sh --serial
```

端末モードは`Ctrl+C`でQEMUを終了します。`halt`後も、QEMUのウィンドウまたはプロセスを終了してください。

## 操作

| コマンド | 動作 |
| --- | --- |
| `help` | コマンド一覧 |
| `about` | カーネルの構成 |
| `mem` | 固定メモリ配置 |
| `echo Hello Rust` | 文字列表示 |
| `calc 12 * 3` | 符号付き64ビット整数の計算 |
| `clear` | 画面消去 |
| `reboot` | 仮想PCを再起動 |
| `halt` | CPU停止 |

演算子は`+`、`-`、`*`、`/`です。各項目を空白で区切ります。除算は整数除算で、ゼロ除算やオーバーフローはエラー表示します。
Backspaceで編集できます。1行は最大127文字で、超過した文字は無視します。履歴、矢印での編集、日本語入力はありません。

## ソースから組み直す

必要なのはPython 3、GNU binutils、Rust 1.90.0のベアメタルターゲットです。Cargoや外部crateのダウンロードはビルドに不要です。
Rustが未導入なら、[公式rustup](https://rustup.rs/)で導入してください。

```sh
sudo apt install binutils python3
rustup toolchain install 1.90.0 --profile minimal --target x86_64-unknown-none
python3 build.py
sh run.sh
```

`rust-toolchain.toml`でコンパイラを固定しています。必要なツールとターゲットの導入後は、オフラインでビルドできます。
`build.py`はブートセクターの512バイト長と署名、カーネルの32 KiB制限、未解決シンボルを検査します。リンカは入口アドレスとスタックとの非重複を検査します。

## 読む順序

| ファイル | 役割 |
| --- | --- |
| `boot.S` | BIOSによる読み込み、A20、32ビット→64ビット、ページテーブル、Rustへの引き渡し |
| `boot.ld` | ブートセクターを`0x7c00`に配置 |
| `src/main.rs` | カーネル入口、VGA、COM1、PS/2、入力ループ、コマンド実行 |
| `src/shell.rs` | 割り当てなしのコマンド解析と検査付き整数計算 |
| `kernel.ld` | カーネルを`0x10000`に配置 |
| `build.py` | コンパイル、リンク、フロッピーイメージ生成 |
| `run.sh` | QEMU起動 |
| `tests/smoke.py` | 実際の起動と入出力を確かめる統合テスト |
| `build/build-info.txt` | ビルドしたサイズ、ツールチェーン、SHA-256 |

## どう起動するか

1. 仮想PCのBIOSが、フロッピーの先頭512バイトを`0x7c00`へ読み込みます。
2. 自作ブートセクターが、続く64セクターを`0x10000`へ読み込みます。
3. A20を有効化し、保護モード、ページング、64ビットモードを設定します。
4. スタックを用意し、Rustの`_start`を呼びます。
5. RustがBSSを初期化し、VGAとCOM1へ文字を出し、入力をポーリングします。

フロッピー形式は1,474,560バイト、18セクター/トラック、2ヘッドの固定配置です。
カーネルの実データは最大32 KiBです。イメージの残りはゼロで埋めています。

| 物理アドレス | 用途 |
| --- | --- |
| `0x1000`〜`0x3fff` | PML4、PDPT、ページディレクトリ |
| `0x7c00`〜`0x7dff` | ブートセクター |
| `0x10000`〜 | カーネル本体・静的領域 |
| `0x80000`〜`0x8ffff` | 予約したスタック領域（下向き） |
| `0xb8000`〜 | VGAテキスト画面 |

最初の1 GiBを2 MiBページで同一アドレスへ対応付けます。この対応付けは実RAMの検出結果ではありません。QEMUのRAMは`run.sh`で64 MiBに設定しています。

## 今回の境界

最小構成の単一タスク・ring 0のカーネルです。割り込みを無効にし、PS/2キーボードとCOM1をポーリングします。
ヒープ、動的メモリ管理、割り込み処理、CPU例外ハンドラ、プロセス分離、システムコール、ファイルシステム、ネットワークは未実装です。
Rustのpanicは表示して停止します。CPU例外の診断機構はまだありません。

次に機能を追加するなら、IDTによる例外の表示、タイマー割り込み、物理ページ管理、タスク切り替えの順に、小さく確認しながら進められます。

## テストを再実行する

```sh
python3 build.py
rustc --edition=2021 --test src/shell.rs -o build/shell-tests
./build/shell-tests
python3 tests/smoke.py
```

`build/`内のテスト結果とスクリーンショットが、そのビルドで行った確認の記録です。

## 一次資料

- [Rust公式: x86_64-unknown-none](https://doc.rust-lang.org/rustc/platform-support/x86_64-unknown-none.html)
- [QEMU公式: 起動オプション](https://www.qemu.org/docs/master/system/invocation.html)
- [QEMU公式: Monitor](https://www.qemu.org/docs/master/system/monitor.html)

これらはターゲット仕様と実行・検証方法の参照資料です。本プロジェクトのOS実装には既存OSのソースを組み込んでいません。
