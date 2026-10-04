# Tane OS — Rustで作る最小の自前カーネル

作成日: 2026 10 02 / 0.9更新: 2026 10 04

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

GitHubの **Code → Download ZIP** でソースを取得して展開するか、リポジトリをcloneし、このREADMEと同じフォルダで実行します。起動イメージは`build/tane-os.img`に同梱済みなので、Rustをインストールしなくても試せます。

```sh
sh run.sh
```

初回は`build/disk.img`（1 MiB、TaneFS用のデータディスク）が作られます。新しいディスクだけ、最初に`file format`してから使ってください。中身は次回以降も残ります。`file format`は既存のファイルをすべて消します。旧TaneFS v1の既存ディスクは読み取り専用で起動し、adminの`file upgrade`でファイルとラベルを保ったままv2にできます。`file status`で版を確認してください。

新しいディスクで、起動後のプロンプトに次のように入力すると、保存と読み出しを試せます。

```text
file format
file write hello "Hello Tane"
file read hello
file list
```

画面をクリックして、`help`と入力してください。入力は英語配列・ASCIIです。マウスとキーボードのキャプチャは`Ctrl+Alt+G`で解除できます。

画面を開けない環境では、端末に接続するモードが使えます。

```sh
sh run.sh --serial
```

端末モードは`Ctrl+C`で入力・sleep・ping・スクリプトを取り消せます。`Ctrl+A`に続けて`X`でQEMUを終了します。QEMUの端末モードでは`Ctrl+A`がQEMUの操作キーなので、シェルへ渡すには2回押してください。`halt`後も`Ctrl+A`、`X`でQEMUを終了してください。

## Tane Shell v1を試す

0.9には、自前のシェル言語、共通operation registry、型付きレコードのパイプライン、入力編集、変数、スクリプト、ファイル変更のplan/apply、ring 3のユーザープロセス、有界なuserヒープ、子プロセス制御、障害後に再実行するストレージの更新記録を含みます。シェルはカーネル内のタスクとして動き、自作の`.tane`実行ファイルを独立したアドレス空間で起動できます。Linuxのシェルや実行ファイルは使いません。

```text
help
help task list
ops | select name effect | take 8
task list | where domain == admin | select pid name state | sort pid --desc
file list | where bytes > 0 | select name bytes | sort bytes | json
file list | count
net status | json
```

`|`で渡すのは、表示済みの文字列ではなく列名・型を持つレコードです。`where bytes > 0`と`sort bytes`は数値として比較します。すべての段の名前・引数・列・型を先に検査するので、不正な後続段があるコマンドで先頭段だけが実行されることはありません。ファイルに残すときは、明示的な末尾の`save`を使います。

```text
task list | select pid name | json | save tasks.json
file read tasks.json
let TARGET 10.0.2.2
net ping $TARGET --count 1
plan file write notes "hello\nnext line"
show
apply
file read notes
status | json
```

`status`は直前のコマンドの結果を`code`・`operation`・`message`として返します。`plan`はファイルのwrite・append・removeに限定した、実行前に確認するための仕組みです。作成ドメイン・ストレージの変更番号・対象ファイルの状態を固定し、`apply`で照合し直します。plan後に別のファイルを変更した場合も、古いplanは拒否します。

`$STATUS`は同じ結果コードを返す読み取り専用変数です。pingのtimeoutは型付きの結果を保ちながら`error`として扱います。`run`は最初の失敗・取消・ドメイン変更で止まり、それまでに完了した変更を残します。`drop`すると、変数・履歴・planを消去します。

詳しい文法・操作・上限・失敗時の扱いは[シェルの使い方](docs/shell-v1.md)にまとめています。

## RustのプログラムをOS上で動かす

`proc run`は、カーネルに同梱した自作Rustプログラムをring 3で動かします。adminのシェルから起動しても、プロセスは必ずuserドメインです。

```text
proc programs
proc run hello
proc list
```

起動時の`started user process as pid ...`に表示された番号で、`let PID 番号`を実行してください。以降は変数で同じプロセスを指定できます。

```text
proc wait $PID
proc output $PID
task list | select pid name domain mode
```

`proc wait`は終了を待ち、出力と結果を読みます。`proc output`はその時点の出力を読むだけです。プロセスは入力中の端末へ直接書き込まず、最大1024バイトの出力を保持します。終了・例外・killの結果は最近8件を残します。`proc wait`のCtrl+Cは待機だけを取り消し、プロセスは動き続けます。一時停止と再開は`proc pause PID`・`proc resume PID`、終了は`task kill PID`を使います。

0.9では、ページ単位のヒープと、プログラムからの子の起動・wait・killも試せます。次を1つずつ起動し、表示されたPIDを`proc wait`へ渡してください。

```text
proc run heap basic
proc run control basic
```

`heap`はゼロ初期化、ページをまたぐsyscall、縮小後の解放と再確保を確認します。`control`は子を起動して、親だけが受け取れる終了結果を待ちます。`proc list`の`frames`と、`proc memory PID`のheap行から使用量を読めます。User全体の24フレーム枠に、親・子・ヒープをすべて含めます。

`proc exec FILE`では、TaneFSの実行ファイルを読み込みます。実行先userにも読み取り権限が必要なため、adminラベルのファイルをadminの権限でuserへ渡すことはできません。新規ファイルをuserラベルでインストールする例です。`drop`は再起動まで元に戻せません。

```text
drop
proc install echo echo.tane
proc exec echo.tane "hello from TaneFS"
```

こちらも、表示されたPIDを変数へ設定し直して`proc wait $PID`を実行してください。メモリ配置、syscall、ファイル権限、プログラムの作り方は[ユーザープロセスの使い方](docs/process-v1.md)にまとめています。

## 操作

| コマンド | 動作 |
| --- | --- |
| `help` | コマンド一覧 |
| `help task list` / `help "task list"` | 1つのoperationの引数・結果・権限・例 |
| `ops` | 同じregistryを型付きレコードで読む |
| `status` | 直前のコマンドの成功・エラー・拒否・取消・commit不明を読む |
| `about` | カーネルの構成 |
| `mem` | メモリ配置、BIOSのE820メモリマップ、物理フレームの使用状況 |
| `alloc` | 4 KiBの物理フレームを1枚確保し、ゼロで埋めてアドレスを表示 |
| `free 0x100000` | `alloc`で得たフレームを返す（16進`0x…`または10進） |
| `ps` | タスク一覧（PID、MACドメイン、状態、CPU時間、カウンター、スタック） |
| `spawn spin` | タスクを起動する（`spin`、`beat`、`once`） |
| `kill 2` | タスクを止め、スタックのフレームを返す |
| `uptime` | 起動からの経過時間（タイマー割り込みの回数から計算） |
| `echo Hello Rust` | 文字列表示 |
| `calc 12 * 3` | 符号付き64ビット整数の計算 |
| `sleep 500` | シェルを指定ミリ秒（0〜60000）だけ眠らせる（その間ほかのタスクが動く） |
| `fault pf` | CPU例外をわざと起こして例外ハンドラやメモリ保護を確かめる（admin専用） |
| `top` | ドメインごとの資源使用量と上限（タスク、フレーム、ファイル、CPU） |
| `disk` | ディスクの状態 |
| `format` | ディスクを初期化して空のTaneFSを作る（admin専用） |
| `ls` | 読めるファイルの一覧（読めないファイルは表示しない） |
| `write NAME TEXT` | ファイルを作る、または内容を置き換える |
| `append NAME TEXT` | ファイルの末尾に追記する |
| `cat NAME` | ファイルを表示する（チェックサムを確かめる） |
| `rm NAME` | ファイルを消す（ディスク上の内容もゼロで消す） |
| `net` / `net status` | NIC、固定アドレス、受信・送信・破棄の件数 |
| `net ping 10.0.2.2` | IPv4のecho要求を3回送信（admin専用） |
| `ping fd00::2 --count 2 --timeout 500` | IPv6のecho要求。回数1〜10、1回の期限1〜5000 ms |
| `sec` | メモリ保護の状態、MACポリシーの表、拒否の件数 |
| `audit` | MACで拒否された操作の記録（新しい16件、admin専用） |
| `drop` | シェルをuserドメインへ下げる（再起動まで戻せない） |
| `clear` | 画面消去 |
| `reboot` | 仮想PCを再起動（admin専用） |
| `halt` | CPU停止（admin専用） |
| `vars` / `let NAME VALUE` / `unset NAME` | シェル内の文字列変数の一覧・設定・削除 |
| `history` | メモリ内の最近8行の入力履歴 |
| `run NAME` | TaneFSのスクリプトを実行（最初の失敗で止める） |
| `plan file write NAME TEXT` | 変更予定を保存。まだ書き込まない |
| `plan file append NAME TEXT` / `plan file remove NAME` | 追記・削除を予定する |
| `show` / `apply [ID]` | 予定を確認・1回だけ適用 |
| `where` / `select` / `sort` / `take` / `count` / `json` | 型付きパイプラインの絞り込み・投影・並べ替え・件数・JSON化 |
| `save NAME` | パイプラインの末尾で、結果をファイルへ保存 |
| `proc programs` / `proc list` | 同梱プログラムと、実行中・最近終了したユーザープロセス |
| `proc run PROGRAM [TEXT]` | 同梱プログラムをring 3・userとして起動 |
| `proc install PROGRAM FILE` / `proc exec FILE [TEXT]` | 実行ファイルを保存・読み込んで起動 |
| `proc wait PID` / `proc output PID` | 終了待ちと結果・出力の読み出し |
| `proc pause PID` / `proc resume PID` | ring 3プロセスを一時停止・再開 |
| `proc memory PID` | プロセスのcode・data・stack・heap配置とページ権限を読む |
| `file status` / `file check` | ファイルシステムの版・復旧状態、読める全ファイルの内容検査 |
| `file sync` | ディスクのflush完了を待つ（admin専用） |
| `file upgrade` | 旧v1をファイル・ラベルを保ってv2に移行（admin専用） |
| `file rename OLD NEW` | 名前を変更。既存の名前は上書きせず、ラベルは保持 |
| `file truncate NAME BYTES` | 0〜4096バイトに伸縮。延長分はゼロ |

演算子は`+`、`-`、`*`、`/`です。各項目を空白で区切ります。除算は整数除算で、ゼロ除算やオーバーフローはエラー表示します。
`fault`の種類は`bp`（ブレークポイント）、`de`（ゼロ除算）、`ud`（未定義命令）、`gp`（一般保護例外）、`pf`（ページフォルト）、`df`（ダブルフォルト）です。
`null`（0番地の読み出し）、`ro`（カーネルのコードへの書き込み）、`nx`（データ領域へのジャンプ）は、ページングによる保護が効いていることを確かめるためのものです。
`bp`はレジスタを表示したあとシェルに戻ります。それ以外はレジスタ（`#PF`では`CR2`も）を表示してCPUを停止します。

`spawn`で起動できるタスクは3種類です。`spin`はCPUを使い続けてカウンターを増やします。タイマー割り込みが来ると、CPUがほかのタスクへ切り替わります。
`beat`は100 msずつ眠ってはカウンターを増やし、眠っている間はCPUを使いません。`once`は300 ms眠ったあと自分で終了し、スタックのフレームが自動で回収されます。
シェルと`idle`を含めて最大8タスクです。シェル（PID 1）と`idle`（PID 0）は止められません。タスクのスタックに使われているフレームは`free`できないので、`kill`で止めてください。

正式な名前は`task list`・`task spawn`・`task kill`、`file list`・`file read`・`file write`・`file append`・`file remove`・`file format`、`net status`・`net ping`です。従来の`ps`・`spawn`・`kill`・`ls`・`cat`・`write`・`append`・`rm`・`format`・`net`・`ping`も、同じoperationへの別名として使えます。

1行の入力は最大255 ASCIIバイトです。左右・Home/End・Backspace/Deleteで編集し、上下で最近8行の履歴を呼び出せます。行末でのTabはregistryのコマンド・変数名・読めるファイル名を補完します。超過した入力は行全体を拒否し、短く切ったコマンドを実行しません。PS/2とCOM1で同じ編集処理を使います。日本語の対話入力、永続履歴、バックグラウンドジョブはありません。pingの実行中は取消以外の入力を捨てます。

## ネットワークを試す

`run.sh`はRTL8139を1台接続します。カーネル自身がPCIを探索し、静的DMAバッファを使ってEthernetを送受信します。受信はPITの10 ms刻みでポーリングし、1回に最大16フレームを処理します。ヒープとNIC割り込みは使いません。

| 設定 | カーネル | QEMU側の仮想ゲートウェイ |
| --- | --- | --- |
| IPv4 | `10.0.2.15/24` | `10.0.2.2` |
| IPv6 | `fd00::15/64`、MACから作るlink-local | `fd00::2` |

```text
net
net ping 10.0.2.2 --count 3 --timeout 1000
net ping fd00::2 --count 2 --timeout 1000
```

IPv4はARP、IPv6はNDPで次の送信先MACを解決し、ICMP/ICMPv6のecho要求を送ります。相手のIP、送信先MAC、識別子、連番、要求固有のpayloadが一致した返信だけを成功として扱います。アドレス解決を含む期限があり、近隣キャッシュは8件・30秒です。プロンプト待ちや`sleep`中も、自分宛てのARP/NDP/echo要求に応答します。ディスク処理など同期コマンド中は受信処理が遅れることがあります。

固定設定は`src/netstack.rs`の`Config::qemu`です。QEMUの標準IPv6ネットワークはこの設定と異なるため、`run.sh`では`ipv6-net=fd00::/64`を明示しています。外部インターネットへのpingはホストとQEMUのICMP制限にも依存します。まず仮想ゲートウェイ、または統合テストの専用peerで確認してください。

ネットワークの解析と実行は`src/netstack.rs`・`src/net.rs`、シェルの実行と表示は`src/shell_runtime.rs`・`src/main.rs`に分けています。pingは型付きの返信、timeoutイベント、完了/取消の集計、機器・プロトコル・権限のエラーを返します。`net status`はほかのシェル操作と共通のregistryに登録され、型付きパイプラインから読めます。

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
`build.py`はユーザープログラムを先にビルドしてから、ブートセクターの512バイト長と署名、カーネルの384 KiB制限、未解決シンボルを検査します。カーネルの機械語も逆アセンブルし、保存するABIのないFPU/SIMD命令を含まないことを検査します。読み込みセクター数は`boot.S`の定数を使い、ローダーと検査の上限を一致させています。リンカは読み込む領域とスタックの非重複、1 MiB以上に置くBSSの上限を検査します。ユーザープログラムだけなら`python3 users/build.py`で組み直せます。

## 読む順序

| ファイル | 役割 |
| --- | --- |
| `boot.S` | BIOSによる読み込み、A20、32ビット→64ビット、ページテーブル、Rustへの引き渡し |
| `boot.ld` | ブートセクターを`0x7c00`に配置 |
| `src/main.rs` | カーネル入口、VGA、COM1、PS/2、入力ループ、コマンド実行 |
| `src/interrupts.rs` | GDT/TSS、IDTと例外表示、PICの再配置、PITタイマー、割り込みからのタスク切り替え |
| `src/frames.rs` | E820メモリマップと4 KiB物理フレームのビットマップ割り当て（ホストでもテスト） |
| `src/sched.rs` | ラウンドロビンの順番、スリープと入力待ちからの起床（ホストでもテスト） |
| `src/tasks.rs` | タスク表、kernel/userの切り替え、CR3とRSP0、スタックの用意と回収、`spawn`/`kill` |
| `src/paging.rs` | カーネル自身のページテーブル（コードは読み取り専用、データは実行禁止、0番地は未対応付け） |
| `src/mac.rs` | 強制アクセス制御のポリシー表と監査ログ（ホストでもテスト） |
| `src/resources.rs` | ドメインごとの資源上限と使用量、CPU配分の計算（ホストでもテスト） |
| `src/security.rs` | すべての要求が通る唯一の関門。ポリシー表と資源上限を確かめ、拒否を監査ログへ記録 |
| `src/ata.rs` | ATA（IDE）ディスクのドライバ（PIO、ポーリング、待ち時間に上限） |
| `src/fs.rs` | TaneFS：ラベルとチェックサム付きの小さなファイルシステム（ホストでもテスト） |
| `src/storage.rs` | ディスクとTaneFSをつなぎ、ファイル操作の前に必ず`security`へ問い合わせる |
| `src/inet.rs` | IPv4/IPv6のアドレス文字列、MAC、Internet checksum |
| `src/pci.rs` | PCI設定空間の探索とI/O BAR・bus master設定 |
| `src/rtl8139.rs` | RTL8139のDMA送受信、リング管理、有限回の待機と復旧 |
| `src/netstack.rs` | Ethernet/ARP/IPv4/ICMP/IPv6/NDP/ICMPv6の純粋な処理 |
| `src/net.rs` | ネットワークサービス、pingの権限判定、期限・取消・型付き結果 |
| `src/shell.rs` | 割り当てなしのコマンド解析と検査付き整数計算 |
| `src/shell_lang.rs` | 引用・エスケープ・変数・パイプラインの有界な構文解析。展開値を再評価しない |
| `src/operations.rs` | operationの名前・別名・引数・効果・結果型・権限を持つ共通registry |
| `src/records.rs` | 型付きの列・セル・レコード、検査済み変換、JSONエスケープ |
| `src/editor.rs` | ASCII入力の編集・履歴・COM1のANSIキー解釈とPS/2共通キー |
| `src/plans.rs` | ファイル変更予定、作成ドメイン・変更番号・対象状態の照合、1回限りのapply |
| `src/shell_runtime.rs` | シェルの共通実行、事前検査、変数・スクリプト・plan・状態の管理 |
| `src/variables.rs` | 有界な文字列変数、名前と容量の検査、読み取り専用STATUSの予約 |
| `src/executable.rs` | 自作Tane実行ファイルの厳密なヘッダー・長さ・入口検査 |
| `src/usermem.rs` | プロセスごとのページテーブル、RX/NX、ガード、有界なヒープの増減、userポインタの全範囲検査とコピー |
| `src/process.rs` | ユーザープロセス、動的フレーム勘定、親子のwait権限、出力1024バイト、最近8件の終了結果、メモリ回収 |
| `src/user_abi.rs` / `src/user_syscalls.rs` | `int 0x80`の独自ABI、コピー・権限・資源検査付きsyscall |
| `src/handles.rs` | PID限定のファイルhandle、固定権限、再利用しないtokenと対象identity |
| `src/user_images.rs` | ビルド済み自作ユーザープログラムの同梱 |
| `users/` | `no_std` Rustユーザープログラム、ABIラッパー、リンカ、ビルドツール |
| `kernel.ld` | カーネルを`0x10000`に配置し、コード・読み取り専用データ・データを4 KiB境界で分ける |
| `build.py` | コンパイル、リンク、フロッピーイメージ生成 |
| `run.sh` | QEMU起動 |
| `tests/host.sh` | 純粋なロジックのモジュールをホストでテストする |
| `tests/smoke.py` | 実際の起動と入出力を確かめる統合テスト |
| `tests/network.py` | 実NICと専用Ethernet peerで通信・不正入力・取消・権限を確認 |
| `tests/shell.py` | QEMU上の型付きパイプライン、編集、変数、スクリプト、plan、権限と保存を確認 |
| `tests/process.py` | QEMU上のring 3、独立メモリ、例外・I/O拒否、syscall・handle・権限・回収を確認 |
| `tests/advanced.py` | QEMU上のヒープ、pause/resume、親子の制御、位置からのファイル操作・保存を確認 |
| `tests/fs_crash.rs` | 本番のTaneFS実装に書き込み・flush・tearの障害を注入して復旧を確認 |
| `tests/process_control.rs` | 本番の親子waitとscheduler経路をホスト上で実行して確認 |
| `build/build-info.txt` | ビルドしたサイズ、ツールチェーン、SHA-256 |

## どう起動するか

1. 仮想PCのBIOSが、フロッピーの先頭512バイトを`0x7c00`へ読み込みます。
2. 自作ブートセクターが、続く768セクターを`0x10000`へ読み込み、BIOSのE820機能でメモリマップを`0x5000`へ保存します。
3. A20を有効化し、保護モード、ページング、64ビットモードを設定します。
4. スタックを用意し、Rustの`_start`を呼びます。
5. RustがBSSを初期化し、自前のページテーブルへ切り替えます（コードは読み取り専用、データはNX、0番地は未対応付け、CR0.WPを有効化）。COM1の有無を確かめ、VGAとCOM1へ文字を出します。
6. E820の使用可能領域のうち、BSSの末尾を4 KiB境界へ丸めた位置から1 GiBまでを、4 KiBフレームとしてビットマップで管理し始めます。
7. 自前のGDT/TSS/IDTを読み込み、PICを割り込み番号32〜47へ移し、PITを100 Hzに設定します。
8. 起動中のコードをシェルタスク（PID 1、ドメインadmin）、`hlt`を繰り返す`idle`タスク（PID 0、ドメインkernel）として用意してから割り込みを有効にします。
9. ATAディスクを探し、TaneFSがあればマウントします。
10. PCIからRTL8139を探し、送受信バッファと固定アドレスを用意します。NICがなくてもシェルは使えます。
11. NICがあるときはシェルが1ティックずつ眠り、入力と受信を調べます。NICがなければ従来の入力待ちを使います。

フロッピー形式は1,474,560バイト、18セクター/トラック、2ヘッドの固定配置です。
カーネルの読み込む実データは最大384 KiBです（`0x10000`〜`0x6ffff`）。1セクターずつ512バイト境界に読むので、フロッピーDMAの64 KiB境界をまたぎません。64 KiBごとに読み込み先のセグメントを進めます。イメージの残りはゼロで埋めています。BSSは`NOLOAD`として1 MiB以上に置き、起動時にゼロ初期化します。フロッピーへゼロのBSSを格納せず、BSSを含む静的RAMはフレーム割り当てから除外します。

| 物理アドレス | 用途 |
| --- | --- |
| `0x1000`〜`0x3fff` | 起動時のPML4、PDPT、ページディレクトリ（カーネルが自前の表へ切り替えた後は使わない） |
| `0x5000`〜`0x560f` | E820メモリマップ（件数と最大64件、ブートセクターが書き込む） |
| `0x7c00`〜`0x7dff` | ブートセクター |
| `0x10000`〜`0x6ffff`以内 | 読み込むカーネルのコード・読み取り専用データ・初期値付きデータ（各保護領域を4 KiB境界で配置） |
| `0x80000`〜`0x8ffff` | シェルタスクのスタック（下向き） |
| `0xb8000`〜 | VGAテキスト画面 |
| `0x100000`〜`__kernel_end` | カーネルのBSS：IDT、TSS、ページテーブル、例外・idle用スタック、フレームのビットマップ、シェル・プロセスの作業領域 |
| BSS末尾を4 KiBへ丸めた位置〜 | 物理フレームとして貸し出すRAM（E820で使用可能な範囲、1 GiBまで） |

最初の1 GiBを同一アドレスへ対応付けます。カーネルのある最初の2 MiBは4 KiBページ、それ以降は2 MiBページです。物理アドレスがそのまま使えるので、フレームもタスクのスタックも物理アドレスで扱います。
E820で予約・ACPIなどとされた範囲が使用可能な範囲と重なる場合は、予約側を優先して使いません。1 GiBを超えるRAMは対応付けていないため管理しません。
QEMUのRAMは`run.sh`で64 MiBに設定しています。使用可能なフレーム数は、E820の予約範囲とそのビルドのBSSサイズを差し引いた数です。起動表示や`mem`で確認できます。

## セキュリティ

### メモリ保護（W^X）

起動直後のページテーブルは全域が読み書き・実行可能です。カーネルは起動時に自前の表へ切り替え、次の保護をかけます。

| 領域 | 保護 | 防ぐもの |
| --- | --- | --- |
| 0番地のページ | 対応付けなし | NULLポインタの読み書き |
| カーネルのコード | 読み取り専用・実行可 | コードの書き換え |
| 読み取り専用データ | 読み取り専用・実行禁止（NX） | 定数の書き換え、データの実行 |
| それ以外すべて（データ、BSS、スタック、フレーム、VGA） | 読み書き可・実行禁止（NX） | データとして置いた内容の実行 |

書き込み可能かつ実行可能なページはありません（W^X）。CR0.WPを有効にし、ring 0でも読み取り専用ページへの書き込みを禁止します。NXはCPUID（`0x80000001`のEDXビット20）で対応を確かめてから使います。
`fault null`、`fault ro`、`fault nx`でそれぞれの保護が効くことを確認できます（ページフォルトとして表示され停止します）。

### 強制アクセス制御（MAC）

すべての要求は`src/security.rs`の1つの関門を通ります。関門は2段階で確かめます。

1. **アクセス制御**（`src/mac.rs`の`POLICY`表）: このドメインに、この種類の対象への、この操作が許されているか
2. **資源の上限**（`src/resources.rs`）: 許されている操作でも、そのドメインに残りの資源があるか

ポリシーは次の1つの表がすべてです。`sec`はこの表をそのまま画面に出すので、表示と実際の判定が食い違うことはありません。

| 主体 | 対象の種類 | 操作 | 対象のラベル |
| --- | --- | --- | --- |
| admin | system | halt reboot fault audit format sync | — |
| admin | memory | alloc free | admin user |
| admin | task | spawn kill | admin user |
| admin | file | create read write delete | admin user |
| admin | network | net-ping | — |
| user | memory | alloc free | user |
| user | task | spawn kill | user |
| user | file | create read write delete | user |

- **表にないものはすべて拒否**: kernelドメイン（`idle`）の対象には誰も手を出せず、kernelドメインを主体とする行もありません。ホストのテストで、表の各行が対象の種類と矛盾しないことも確かめています。
- **ラベル**: `task spawn`のカーネルタスク、`alloc`のフレーム、作成するファイルは、作ったタスクのドメインをラベルとして持ちます。`proc run`/`proc exec`のプロセスは必ずuserです。ファイルのラベルはディスクに書かれ、再起動後も有効です。
- **一方向の降格**: `drop`でadminからuserへ下げられますが、上げる方法はありません。戻るには再起動が必要で、userドメインには再起動も許されていません。
- **継承**: `task spawn`した信頼済みのカーネルタスクはシェルのドメインを引き継ぎます。ユーザープロセスはadminから起動してもuserになり、userのタスク・フレーム上限を使います。
- **見えないものは見せない**: `ls`は読めるファイルだけを表示します。
- **監査**: ポリシー、資源上限、read/writeでのhandle権限・他PIDのtoken・古いidentityによる拒否を、時刻・PID・ドメイン・操作・対象・理由とともに1つのログに記録します（新しい16件と通算件数）。`audit`で読めるのはadminだけで、件数は`sec`で誰でも確認できます。
- **通信**: 要求されたpingは`src/net.rs`の入口で判定し、userの要求はARP/NDPも送る前に拒否します。状態表示は誰でも使えます。カーネル内部の自分宛てARP/NDP/echoへの応答は要求されたpingとは別です。宛先ごとの権限やソケットはまだありません。
- **判定と実行の一体化**: タスク・フレーム・ファイルは、対象のラベルを調べる処理、判定、実行を、割り込み禁止の同じ区間で行います。シェルとuserのsyscallからのファイル操作は、同じstorage関門を使います。
- **`free`の対象限定**: `free`できるのは`alloc`で得たフレームだけです。

### 計算資源の上限

| ドメイン | タスク | フレーム | ファイル | CPU配分 |
| --- | --- | --- | --- | --- |
| admin | 6 | 4096（16 MiB） | 24 | 70% |
| user | 3 | 24（96 KiB） | 8 | 30% |

カーネルのデモタスクはスタックに4フレーム、ユーザープロセスはページテーブル4・コード1・データ1・userスタック2・kernelスタック4の基本12フレームを使います。ヒープはさらに0〜8フレームを使い、同じUserの勘定に加算します。userの上限24フレームから、ヒープなしのユーザープロセスは同時に最大2個です。1個でもヒープを使っていれば、ほかのUser資源がなくても2個目の基本12フレームを確保できません。子プロセスも同じ上限に従います。

- **CPU配分**: 1秒（100ティック）ごとに、動的に起動したkernel taskとuser processが使ったティックをドメインごとに数えます。配分を使い切ったドメインのタスクは、ほかのドメインに実行待ちのタスクがある間は後回しになります。ほかに待つタスクがなければ配分を超えても動き、CPUを遊ばせません。adminとuserのタスクが競うと、70%と30%に分かれます（`top`で確認できます）。
- **数えないもの**: シェルと`idle`は数えず、後回しにもしません。シェルの応答を保つためです。
- **上限超過の扱い**: 上限を超える要求は`quota`として拒否し、監査ログに残ります。

### ストレージ（TaneFS v2）

QEMUのIDEディスク（`run.sh`では`build/disk.img`、1 MiB）を、自作のATAドライバで読み書きします。新規formatはv2で、旧v1ディスクは自動変換せず読み取り専用でマウントします。adminが明示的に`file upgrade`すれば、ファイルを保ってv2へ移行できます。

| 場所 | 内容 |
| --- | --- |
| LBA 0 | スーパーブロックと配置・全headerチェックサム |
| LBA 1〜4 / 5 | ファイル表32件 / 表4セクターのチェックサム一覧 |
| LBA 8〜263 | データ。1ファイル8セクター・最大4 KiB |
| LBA 264〜274 | redo journal。commit headerと、変更後のdata・表・checksum一覧 |

- **更新と復旧**: 変更後の内容・表・checksumをjournalへ保存してflushし、commit印をflushしてから本来の場所へ反映します。途中で停止したcommit済み更新は、次のマウントで全payloadを検査して再実行します。
- **破損の検出**: 表のchecksumをマウント時に、内容のchecksumを読み取り時に検査します。壊れたcommit印やpayloadを部分適用せず、マウントを拒否します。
- **名前・長さ・部分書き込み**: シェルは`file rename`・`file truncate`を使えます。userのhandleはseek・位置からのwrite・truncateも使え、空いた部分や延長分をゼロにします。
- **再利用時の消去**: 変更後の4 KiB領域全体を用意し、EOF後ろをゼロにします。deleteは全体のゼロと空の表を1つの更新として記録します。
- **移行**: `file upgrade`は全ファイルの内容と既存表を検査してから補助metadataを保存し、最後にv2のスーパーブロックを公開します。file本体・ラベル・世代を変えません。
- **状態と検査**: `file status`で版・readonly・復旧の有無を読み、`file check`で現在のドメインが読める全ファイルの内容を検査できます。`file sync`はadmin専用のflushです。
- **保証の前提**: durableなセクター書き込みとflushをデバイスが守ることを前提にします。任意のtearや故障から必ずマウントできる保証、暗号学的な偽造防止、形式証明はありません。format自体は破壊的で、途中で止まれば未formatになり得ます。

ディスク配置、commitの順序、v1の扱いと障害時の制約は[ストレージv2](docs/storage-v2.md)を参照してください。

### オブジェクトの再利用

`alloc`で渡すフレームと、タスクに渡すスタック（16 KiB）は、渡す前に必ずゼロで埋めます。ディスクの領域も同じです（前項）。前の持ち主（別ドメインのタスクを含む）のデータは見えません。

### 限界

シェル・ドライバ・`task spawn`のデモタスクは信頼済みのring 0コードで、カーネルのアドレス空間を共有します。そのコード自身の不具合や直接アクセスを、MACで隔離することはできません。
`proc`で起動するプログラムにはring 3、独立したCR3・物理ページ、RX/NX、スタックガード、syscallでの全ポインタ範囲検査を使います。ファイル権限はPIDごとではなくadmin/userのドメイン単位なので、userプロセス同士のファイルの秘密性はありません。同じuserのプロセス結果・出力もドメインの読み取りポリシーに従います。

## 今回の境界

信頼済みのカーネルタスクと、隔離された小さなユーザープロセスを動かします。

- `task spawn`はカーネル内の関数、`proc run`/`proc exec`は独立したページテーブルを持つring 3コードを動かします。終了・kill・user例外で資源を回収し、使い終わったページとkernelスタックをゼロにします。
- タスク切り替えはすべて割り込みの中で行います。タイマー割り込み（10 msごと）で次の実行可能なタスクへ順に切り替え、CR3とTSSのRSP0も更新します。kernel taskは`int 48`、user processは`int 0x80`のyield/sleepを使います。
- 各kernelスタックの底に目印の値を置き、切り替えのたびに確かめます。書き換わっていたらスタックあふれとしてpanicで停止します（この検出はテストで再現していません）。userスタックには未対応付けのガードを使います。
- タスク表とフレームの割り当ては、割り込みを禁止した区間でだけ変更します。タスクがその区間の途中で横取りされることはないため、`kill`しても割り当て状態が壊れません。
- CPU例外（0〜31番）はすべてIDTで受けます。ring 3の例外はそのプロセスだけを終了し、`proc wait`で結果を読めます。ring 0の例外はレジスタを表示して停止します（kernelの`#BP`だけは表示後に復帰）。
- ダブルフォルトはTSSのIST1にある専用スタックで処理するため、スタックが壊れていても三重フォルト（再起動）にならず表示できます。
- 割り込みはPITタイマー（IRQ 0、100 Hz）、キーボード（IRQ 1）、COM1受信（IRQ 4）だけを受け付けます。キーボードとCOM1の割り込みは入力待ちのシェルを起こすためだけに使い、データはシェルが読みます。割り込みを取りこぼしても止まらないよう、入力待ちは100 msごとにも見直します。
- COM1がない構成（`sh run.sh`の画面モード）では、COM1を使わずにVGAとキーボードだけで動きます。

ユーザープロセスの実行ファイルは固定配置のコード1ページ・データ1ページ・userスタック2ページです。ヒープだけは固定した仮想領域を0〜8ページに増減できます。任意アドレスのmmap、ELF/POSIX互換、fork、pipeで接続した外部コマンド、ディレクトリはありません。CPUの拡張状態を保存するABIがないため、x87/MMX/SSE/AVXは使用不可です。ハードウェアTSを有効化し、XSAVEとFSGSBASEを無効化しています。TLSもありません。
ネットワークはRTL8139が1台、MTU 1500、固定アドレス、ARP/NDPとechoのみです。DHCP、DNS、SLAAC、IPv6 DADによる自アドレス重複検査、UDP、TCP、ソケット、VLAN、IPv4断片の再構成、IPv6拡張ヘッダは未実装です。相手のDAD要求への応答は行います。
Rustのpanicは表示して停止します。

シェルv1はカーネルの機能を一貫した入口から使い、ユーザープロセスは独自syscallとTane形式のプログラムを実行します。プログラムの標準出力は有界な記録で、シェルの型付きパイプラインとは別です。

## テストを再実行する

```sh
python3 build.py
sh tests/host.sh
python3 tests/smoke.py
python3 tests/network.py
python3 tests/shell.py
python3 tests/process.py
python3 tests/advanced.py
```

`build/`内のテスト結果とスクリーンショットが、そのビルドで行った確認の記録です。

| 検証 | 確認する内容 | 記録 |
| --- | --- | --- |
| `tests/host.sh` | 純粋なロジック、シェル構文と展開、registry、型付き変換、入力編集、planの状態照合 | `build/host-summary.txt` |
| `tests/smoke.py` | BIOSからの起動、例外、タスク、メモリ、MAC、資源制限、TaneFS | `build/smoke-summary.txt` |
| `tests/network.py` | 実RTL8139、IPv4/IPv6、異常フレーム、取消、userから送信しないこと | `build/network-summary.txt` |
| `tests/shell.py` | 型付き処理、引用と変数の非評価、編集、save、スクリプトの停止、planと権限 | `build/shell-summary.txt` |
| `tests/process.py` | 実CPL3、独立CR3、プリエンプション、user例外、syscallとhandle、Userの資源上限・ファイル権限・回収 | `build/process-summary.txt` |
| `tests/advanced.py` | 有界ヒープ、停止と再開、親子のwait/kill、positioned I/O、ストレージの権限と保存 | `build/advanced-summary.txt` |

## 一次資料

- [Rust公式: x86_64-unknown-none](https://doc.rust-lang.org/rustc/platform-support/x86_64-unknown-none.html)
- [QEMU公式: 起動オプション](https://www.qemu.org/docs/master/system/invocation.html)
- [QEMU公式: Monitor](https://www.qemu.org/docs/master/system/monitor.html)
- [QEMU公式: ネットワーク](https://www.qemu.org/docs/master/system/devices/net.html)
- [Realtek: RTL8139C仕様書](https://people.freebsd.org/~wpaul/RealTek/spec-8139c(160).pdf)
- [RFC 826: ARP](https://www.rfc-editor.org/rfc/rfc826.html)
- [RFC 792: ICMP](https://www.rfc-editor.org/rfc/rfc792.html)
- [RFC 4443: ICMPv6](https://www.rfc-editor.org/rfc/rfc4443.html)
- [RFC 4861: IPv6 Neighbor Discovery](https://www.rfc-editor.org/rfc/rfc4861.html)

これらはターゲット仕様と実行・検証方法の参照資料です。本プロジェクトのOS実装には既存OSのソースを組み込んでいません。
