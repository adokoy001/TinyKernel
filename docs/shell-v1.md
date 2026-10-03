# Tane Shell v1

更新: 2026 10 04 / Tane OS 0.7

Tane OSの機能を、同じ名前・引数・権限で対話操作とスクリプトから使うための小さなシェルです。Rustの`core`だけで動き、ヒープ、外部crate、ホストのシェルを使いません。LinuxやPOSIXとの互換性を前提にしない、カーネル内のシェルタスクです。

## 起動して最初に試す

```sh
sh run.sh
# 画面を開けない環境なら:
sh run.sh --serial
```

新しいデータディスクにはTaneFSがないため、初回だけ`file format`します。既存ディスクでこの操作をすると、すべてのファイルが消えます。

```text
help
help task list
ops | select name effect | take 8
task list | select pid name domain state
file list | select name bytes | sort bytes
net status | json
```

`help`はoperation registryから一覧を生成します。`help task list`と`help "task list"`は同じoperationを説明します。`ops`は同じregistryをレコードとして返すため、名前、効果、結果型、権限をパイプラインで調べられます。

## 名前と別名

| 正式な名前 | 従来の別名 | 動作 |
| --- | --- | --- |
| `task list` | `ps`、`tasks` | タスクの一覧 |
| `task spawn spin\|beat\|once` | `spawn` | デモ用タスクの起動 |
| `task kill PID` | `kill` | タスクの終了 |
| `file list` | `ls` | 読めるファイルの一覧 |
| `file read NAME` | `cat` | 内容の読み出し |
| `file write NAME [TEXT]` | `write` | 作成、または内容の置き換え |
| `file append NAME TEXT` | `append` | 追記 |
| `file remove NAME` | `rm` | 削除 |
| `file format` | `format` | ファイルシステム全体の初期化 |
| `net status` | `net` | NICと固定アドレスの状態 |
| `net ping IP [--count N] [--timeout MS]` | `ping` | ICMP echoによる到達確認 |

別名も同じoperationと同じ権限検査を使います。ほかに`about`、`mem`、`uptime`、`alloc`、`free`、`top`、`sec`、`audit`、`drop`、`echo`、`calc`、`sleep`、`fault`、`clear`、`reboot`、`halt`があります。引数と権限は`help 名前`で確認してください。

registryの効果は`read`（参照）、`write`（ファイル・資源・変数などの変更）、`control`（制御や通信）です。効果の表示自体が権限を与えることはありません。実行は通常のカーネル側の検査を通ります。

## パイプは型付きレコードを渡す

`|`は画面に表示した表を再解析する仕組みではありません。列名と型を持つレコードを次の段へ渡します。文字列、符号付き整数、符号なし整数、真偽値、`null`を区別し、整数列の比較や並べ替えを数値として行います。

```text
task list | where domain == admin | select pid name state | sort pid --desc
file list | where bytes > 0 | select name bytes | sort bytes | take 5
file list | where bytes >= 10 | count
file list | json
mem | json
top | json
audit | json
net status | json
ops | select name effect result
net ping 10.0.2.2 --count 1 | json
```

`audit`はadmin専用です。userに下げた後でも、パイプを使って権限を迂回することはできません。ファイル一覧には現在のドメインで読めるものだけを出します。

| 変換 | 意味 |
| --- | --- |
| `where FIELD OP VALUE` | 列の型に従って比較する。`OP`は`==`、`!=`、`<`、`<=`、`>`、`>=` |
| `select FIELD ...` | 指定した列を指定順に残す |
| `sort FIELD` | 1列で昇順に並べる |
| `sort FIELD --desc` | 1列で降順に並べる |
| `take N` | 先頭から最大N行を残す。Nは0〜64 |
| `count` | 入力の行数を返す |
| `json` | 列名・型・値をJSONへ変換する。引用符、改行などはエスケープする |
| `save NAME` | 末尾で使い、結果をファイルへ保存する |

`where`の数値は列の符号・範囲に従い、文字列の比較は文字列として行います。存在しない列、同じ列の重複選択、型に合わない値はエラーです。`<`と`>`が演算子になるのは、文字通りの`where`の第3語に置いた独立した比較演算子だけです。`echo hi > file`のようなリダイレクトは使えません。

パイプライン全体のoperation・引数・列・型・saveの位置を、ソースや保存先の実行前に検査します。たとえば次の行は、後半の列が存在しないのでファイルを書きません。

```text
file list | select nonexistent | save should-not-exist
```

書き込みやtask起動などの操作を、レコードソースとしてパイプの先頭へ置くこともできません。通常の表示と保存には同じデータを使います。

例外として、回数と時間を制限した`net ping`は送信を伴うソースとして使えます。権限は通常のpingと同じです。1件でもtimeoutがあると結果コードは`error`になりますが、`timeout`と`summary`の型付きレコードはJSON化・保存できます。スクリプトでは、そのpingの行の後で止まります。

```text
task list | select pid name | json | save tasks.json
file read tasks.json
echo "literal text" | save greeting
```

`json`なしでレコードをsaveすると、列名を持つTSVとして保存します。変換していない`echo`・`file read`のテキストは連結した本文を保存し、`echo`には末尾の改行を付けます。変換していない`calc`は数値と改行を保存します。JSONとして残す場合は、例のように`json | save NAME`を明示します。`json`は最後の表示段で、その後に置けるのは`save`だけです。

主要な列は次のとおりです。ほかのソースの列は、そのソースを直接実行して確認できます。

| ソース | 列 |
| --- | --- |
| `task list` | `pid`、`name`、`domain`、`state`、`cpu_ticks`、`counter`、`stack` |
| `file list` | `slot`、`label`、`bytes`、`generation`、`name` |
| `ops` | `name`、`effect`、`result`、`permission` |
| `status` | `code`、`operation`、`message` |
| `history` | `id`、`part`、`command` |
| `vars` | `name`、`value` |

型付きレコードの上限は64行、8列、1セルの文字列はUTF-8で64バイトです。超過を黙って切り詰めません。`save`の結果はTaneFSの1ファイル上限4 KiBにも従います。

`echo`・`file read`の長いテキストは、UTF-8の文字を壊さず64バイト以下の`text`レコードに分けます。`history | json`も、255バイトまでの入力を64バイト以下の`command`へ分割し、同じ`id`と1から始まる`part`で区別します。`id`は現在の履歴内で古い行から付ける順番号です。`history | count`は分割後のレコード数で、元の入力行数とは限りません。直接の`history`表示と上下キーの履歴は、分割していない入力行を使います。

## 引用、エスケープ、変数

空白は語を区切り、引用は1つの語を作ります。隣り合う未引用・引用の部分は連結します。空の引用`""`や`''`も、空文字列の1語として残ります。

```text
echo "hello world"
echo 'pipe | and $NAME remain literal'
echo pre" two"' parts'
file write notes "first line\nsecond line\tindented"
```

単引用の中ではすべてが文字通りのデータです。二重引用では`\n`、`\t`、`\r`、`\\`、`\"`、`\'`、`\$`と変数展開を使えます。未引用でもバックスラッシュで空白や`|`などを文字として渡せます。不明なエスケープ、閉じていない引用はエラーです。

```text
let TARGET 10.0.2.2
net ping $TARGET --count 1
let PAYLOAD 'a | file remove notes; $(halt)'
echo $PAYLOAD
echo "target=${TARGET}"
vars
unset TARGET
```

変数は文字列だけで、最大8個、名前は最大24 ASCIIバイト、値は最大64 UTF-8バイトです。名前は英字または`_`で始まり、後ろには数字も使えます。大小文字を区別します。未定義の名前はエラーです。`$NAME`と`${NAME}`が使えます。

展開した値は1語のデータで、空白で再分割せず、`|`や引用符を構文として読み直さず、その中の`$NAME`を再帰展開しません。operation名や複合名の部分を変数から作ることは拒否します。たとえば`let CMD halt`の後でも`$CMD`は実行できません。

`$STATUS`は予約された読み取り専用変数で、直前の結果コードを返します。`let STATUS ...`と`unset STATUS`は拒否します。`status`の型付きレコードでは、結果コードとoperationと説明を一緒に読めます。

```text
file read missing
echo $STATUS
status | json
```

結果コードは`success`、`error`、`denied`、`cancelled`、`commit-unknown`です。`echo $STATUS`自体が成功すると、次のコマンドから見る結果はその成功になります。

`status`と保存を伴わない`status`のパイプラインは、表示しただけでは直前の状態を上書きしません。空行とコメントもその状態を保ちます。

`*`は文字通りのデータで、ファイル名のglob展開はしません。`&&`、`||`、`;`、リダイレクト、未引用のバッククォートや`$(...)`によるコマンド置換はありません。引用した置換らしい文字列はデータです。コメントは、行の最初の非空白文字が`#`のときだけです。`echo # text`の`#`はデータになります。

構文解析は1行512バイト、展開後の語の合計512バイト、48語、8段までです。対話入力の行はエディターの255バイト制限も受けます。

## ファイル変更を確認してから適用する

`plan`が扱うのは、1件のファイルのwrite・append・removeです。予定はシェル内に1件だけ保持し、ディスクには保存しません。新しいplanの作成に成功すると、前の予定を置き換えます。

```text
plan file write notes "replacement text"
show
apply
```

`show`でID、作成ドメイン、対象、予定した内容と変更前の状態を確認できます。`apply ID`とIDを明示することもできます。`plan halt`や`plan file format`、複数の操作をまとめたplanは対応していません。

planは、単なるコマンド文字列の保存ではありません。作成ドメイン、その起動中だけ有効なストレージ全体の変更番号、対象が存在するか、存在する場合のslot・ラベル・サイズ・世代・チェックサムを固定します。既存ファイルは読み取り権限と内容のチェックサムを確認してから予定します。apply時も権限とチェックサムを確認し、同じ対象かを照合します。

別のファイルへの書き込み、削除、format、再マウントも変更番号を進めるため、対象の見た目が同じでも古いplanを拒否します。adminで作ったplanを、`drop`後にuserとして表示・適用することもできません。

```text
plan file write stale "this must not be applied"
file write other "intervening change"
apply
```

このapplyはエラーになります。applyは1回の試行で予定を消費します。IDの不一致、権限拒否、古い状態、チェックサムの不一致、ディスクエラーでも、繰り返し実行できる予定として残しません。もう一度行うには、現在の状態からplanし直します。

ディスクへの変更中にI/Oが失敗すると、どこまで書き込まれたか確定できないため`commit-unknown`を返し、マウント中の状態を無効にします。成功したようには報告しません。この小さなファイルシステムはジャーナルや自動ロールバックを持たず、planも電源断時の原子性や復旧を保証するものではありません。

## スクリプト

TaneFSのASCIIテキストファイルを`run NAME`で実行します。1行が1パイプラインです。改行をファイルへ入れるには、二重引用の`\n`を使えます。LFとCRLFを受け付けます。

```text
file write demo.tsh "# demo\necho begin\ntask list | select pid name\nsleep 100\necho done"
run demo.tsh
```

実行前に、そのファイル全体の引用・演算子・既知のoperation名を検査します。後ろに未知のコマンドや不正な構文があれば、そのファイルの最初の行も実行しません。変数をoperation名に使うことは、この段階でも拒否します。

実行は行ごとに現在の変数で展開し、パイプラインと権限を検査します。最初の実行エラーで止まります。前の行で成功した書き込みやtask起動を取り消すトランザクションではありません。

```text
file write stop.tsh "file write first yes\nfile read absent\nfile write later no"
run stop.tsh
```

この例では`first`の書き込みは残り、`later`は書きません。未定義変数、読み取り権限、ディスク状態などは実行時にも失敗し得ます。

1ファイルのコマンド行は32行までで、空行とコメント行は数えません。入れ子のrunは深さ4、1回のrunから全ネストで実行するコマンド行は合計128行までです。スクリプト内のrun呼び出し行も数えます。自分を呼ぶスクリプトもこの上限で止まります。Ctrl+Cは行と行の間、およびsleep・pingの待機中に取り消せます。すでに完了した変更は残ります。同期ディスクI/Oを途中で巻き戻す仕組みはありません。

スクリプトが`drop`でドメインを変更した場合は、その行の後で止まり、adminで読み込んだ後続行をuserとして実行しません。読み込んだスクリプトのバッファは、成功・失敗・取消のどの終了経路でも消去します。

## 入力編集

対話入力は最大255 ASCIIバイトです。PS/2キーボードとCOM1端末で同じエディターを使います。上限を超えた行は全体を拒否し、先頭部分だけをコマンドとして実行しません。Ctrl+Cで入力を取り消して、新しい行を始められます。

| キー | 動作 |
| --- | --- |
| 左右 | カーソル移動 |
| Home / Ctrl+A | 行頭へ |
| End / Ctrl+E | 行末へ |
| Backspace | 手前の文字を削除 |
| Delete / Ctrl+D | カーソル位置の文字を削除 |
| 上 / Ctrl+P | 前の履歴を呼び出す |
| 下 / Ctrl+N | 次の履歴・編集中の行へ戻る |
| Ctrl+B / Ctrl+F | 左 / 右へ移動 |
| Ctrl+U | カーソルより前を削除 |
| Ctrl+K | カーソル以降を削除 |
| Ctrl+W | 手前の語を削除 |
| Tab | registryのコマンド・変数名・読めるファイル名の補完 |
| Ctrl+C | 入力または取消可能な処理を取り消す |

QEMUの`--serial`モードではCtrl+AがQEMUに使われます。シェルの行頭操作にはHomeを使うか、Ctrl+Aを2回押してください。QEMU終了はCtrl+Aの後にXです。

履歴は最近8行をメモリ内に保持し、`history`でも読めます。再起動では消えます。日本語IME、永続履歴、バックグラウンドジョブ、外部実行ファイルはありません。

Tab補完はカーソルが行末にあり、引用・エスケープ・パイプを含まない入力に対応します。候補が1件なら補い、複数なら共通の部分を補うか候補を表示します。候補を実行することはありません。ファイル名の候補にも現在のドメインの可視性を使います。

`drop`に成功すると、変数、入力履歴、保留中のplan、作業用レコードを消去します。adminのシェル状態を、そのままuserへ持ち越しません。

## カーネル側の境界

MAC・ファイルラベル・資源上限は、正式名、別名、パイプライン、スクリプト、applyからの要求でも同じカーネルの入口で確認します。userからのpingはARP/NDPを送る前に拒否します。変数や保存したplanを権限そのものとしては扱いません。

ただしすべてのコードはring 0で動き、アドレス空間を共有します。現在のデモ用taskはカーネル内の関数で、悪意ある機械語を隔離して実行する環境ではありません。ユーザーモード、独立したページテーブル、システムコール、プロセス分離は未実装です。

ネットワークはRTL8139、固定IPv4/IPv6、ARP/NDP、ICMP echoの範囲です。UDP、TCP、DNS、DHCP、ソケット、POSIX互換はありません。これはシェルが存在することによって追加される機能ではなく、別に実装するカーネル機能です。

## 検証

```sh
python3 build.py
sh tests/host.sh
python3 tests/smoke.py
python3 tests/network.py
python3 tests/shell.py
```

hostテストは構文・上限・展開値による注入の防止、registryの一貫性、型付きレコード、編集、planの状態照合を確認します。`tests/shell.py`は実際にBIOSからQEMUを起動し、COM1の入力とATAディスクを使って、パイプライン、引用・変数、save、スクリプトの停止・取消、planの失効、権限を確認します。結果は`build/`内のsummaryとserial記録に残します。
