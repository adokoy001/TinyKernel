# ユーザープロセスとメモリ管理

更新: 2026 10 04 / Tane OS 0.9

自作Rustプログラムを、Tane OSのring 3で実行します。プロセスごとにページテーブルと物理ページを持ち、カーネルへの要求は独自の`int 0x80` syscallを通します。シェルの`task spawn`で起動する信頼済みring 0のデモタスクとは、実行するCPU権限とメモリ配置が違います。

## 同梱プログラムを動かす

QEMUを`sh run.sh`または`sh run.sh --serial`で起動して、シェルへ入力します。同梱プログラムの起動には、データディスクをformatする必要はありません。

```text
proc programs
proc run hello
proc list
```

起動時に`started user process as pid ...`と番号が表示されます。その数字を使って`let PID 番号`を実行してください。以降は変数で、同じプロセスを指定できます。

```text
proc wait $PID
proc output $PID
status
```

`hello`は挨拶と自分のPIDを出力し、exit 0で終了します。waitは出力と終了理由を表示します。PIDは起動した順番で変わるため、表示された番号を使ってください。

次の例も、起動したPIDを変数へ設定してからwaitできます。

```text
proc run echo "text | still data"
proc run sleep 100
```

引数は128バイトまでの1つの文字列です。空白を含む場合は引用します。`|`、`$()`、引用符などが引数へ渡されても、そのプログラムのデータで、シェル構文として再評価しません。`proc run`と`proc exec`は起動するとプロンプトへ戻り、終了を待ちません。

| 操作 | 動作 |
| --- | --- |
| `proc programs` | 同梱プログラムの名前とTane形式のバイト数 |
| `proc list` | 実行中と、最近終了したプロセスの状態 |
| `proc memory PID` | code・data・stack・heapの配置とページ権限 |
| `proc run PROGRAM [TEXT]` | 同梱プログラムを起動 |
| `proc install PROGRAM FILE` | 同梱プログラムをTaneFSへ保存 |
| `proc exec FILE [TEXT]` | TaneFSのTane実行ファイルを読み込んで起動 |
| `proc wait PID` | 終了を待ち、出力・終了コード・例外・killの結果を読む |
| `proc output PID` | 現在までの出力を読む。終了待ちはしない |
| `task kill PID` / `kill PID` | 実行中のプロセスを終了 |
| `proc pause PID` / `proc resume PID` | ring 3プロセスの実行を一時停止・再開 |

`proc wait`のCtrl+Cは待機だけを取り消します。待っていたプロセスをkillしたり巻き戻したりはしません。実行を止める必要がある場合は、`task kill $PID`を使います。exit 0ならwaitの結果は`success`、非0終了・CPU例外・killなら`error`です。スクリプトも、この結果に従って最初の失敗で止まります。

## 状態と出力

```text
proc programs | json
proc list | select pid name domain state frames
task list | select pid name domain mode
proc output $PID | json
```

`task list`の`mode`は`kernel`または`user`です。`proc list`の列は`pid`、`parent`、`name`、`domain`、`state`、`cpu_ticks`、`frames`、`output_bytes`です。実行中は`ready`・`running`・`sleeping`・`waiting`、一時停止中は`stopped`、完了後は`exited`・`faulted`・`killed`を使います。終了コードや例外の詳細はwaitで読みます。

出力は、入力中の端末へ非同期に表示しません。プロセスごとに、成功したstdout書き込みの合計を1024バイトまで保持します。1回のsyscallでは最大256バイトです。残り容量に収まらない書き込みは全体を拒否し、`EAGAIN`を返して「書き込みを拒否した」印を結果へ付けます。その書き込みの一部だけを成功として扱ったり、以前の成功した出力を消したりはしません。

outputとwaitは保持した出力をコピーして読むだけです。読んでも出力容量は空にならず、同じ内容を何度でも読めます。改行、タブ、表示可能ASCII以外は`\xNN`へ変換するため、ESCなどの制御コードでシェルやホスト端末の表示を操作できません。

終了・例外・killの結果と出力は最近8件を残します。新しい結果で古いものを置き換え、置き換えた古い出力を消去します。結果はディスクへ自動保存せず、再起動で消えます。liveプロセスのメモリは、終了結果の記録とは別に回収します。

`proc run`/`proc exec`/`proc install`/`proc wait`/`proc pause`/`proc resume`はシェルのレコードソースにできません。プログラムの状態と出力を取り込むには`proc list`・`proc memory`・`proc output`を使います。`proc memory`は生存中のプロセスだけが対象で、回収済みの終了結果からページを読む操作はありません。プロセス同士をPOSIXのpipeで接続する機能はありません。

## 実行を一時停止する

```text
proc run busy
# 表示されたPIDをlet PID 番号で設定してから:
proc pause $PID
proc list | select pid state cpu_ticks frames
proc memory $PID | select region base bytes pages read write execute
proc resume $PID
task kill $PID
```

pauseはring 3のプロセスだけに使い、killと同じMAC権限を確認します。シェル・idle・`task spawn`のring 0タスク、終了済みのPIDは対象にできません。すでに停止したPIDのpauseや、停止していないPIDのresumeはエラーです。

停止中もメモリとhandleは保持します。sleepの期限は実時間どおりに進みますが、期限に達してもresumeまでは実行しません。期限より前にresumeした場合は、残り時間を眠ります。子のwait中に停止した親も、子が終了しただけでは再開せず、resumeを待ちます。pauseは巻き戻しや資源の回収ではありません。

## プログラムから子プロセスを起動する

ring 3からもsyscall 15で同梱プログラム、またはUserが読めるTaneFS実行ファイルを起動できます。新しいコード・データ・スタックをゼロ初期化した独立アドレス空間に用意し、名前と引数はコピーしてから起動します。親のメモリ、heap、ファイルhandleを引き継ぎません。

1つのUserの親が持てる、まだwaitで受け取っていない子の関係は1件です。子が実行中でも終了済みでも、waitを成功させる前の次のspawnは`EAGAIN`で拒否します。さらにUser全体の24フレーム枠を使うため、基本12フレームの親と子を動かす間は追加ヒープを使えません。

syscall 16は、自分が起動した正のPIDの子だけをwaitします。PID 0、任意の子を選ぶwait、非同期のwaitは対応していません。子がまだ実行中なら親は`waiting`となり、子の終了時に40バイトの結果を指定先へコピーして、RAXへ子のPIDを返します。終了済みなら同じ結果をすぐ返します。待機はCPUを使い続けるループではなく、スケジューラのblocked状態です。

結果を受け取る範囲の書き込み権限は、待機や消費の前に全体を検査します。不正なポインタなら`EFAULT`となり、子の結果は消費しません。成功したwaitは子の関係を1回だけ消費し、もう一度同じPIDをwaitすると`ECHILD`になります。親に保存する終了理由はシェルの最近8件とは別なので、その一覧から古い結果が押し出されても親のwaitは失敗しません。stdoutはこの40バイトには含みません。

syscall 17で終了できるのも、自分の実行中の子だけです。同じUserドメインでも、他の親が起動したPIDへのwaitやkillは`ECHILD`で拒否します。信頼済みシェルの`proc wait`・`task kill`はドメインの権限で操作するため、このring 3用の親子制限とは異なります。

親がexit・例外・killで終了すると、子はそのまま実行を続けます。子のwaitを受け取る権限は失効し、PIDを再利用しないため、後から同じtask slotを使う別の親へ渡しません。孤児になった子の結果は通常のシェル一覧に残ります。親の終了に連動したkill、子の再養子、POSIXのsignalはありません。

## 実行ファイルをTaneFSに保存する

新しいデータディスクは、adminのシェルで最初に`file format`してTaneFS v2を作ります。旧v1ディスクは読み取り専用で、install・writeなどは拒否します。adminの`file upgrade`で既存のファイルを保ってv2へ移行できます。移行と障害後の復旧は[ストレージv2](storage-v2.md)を参照してください。既存のファイルがあるディスクはformatしないでください。インストールには通常のファイル権限と容量制限を使います。

```text
proc install hello adminhello.tane
proc exec adminhello.tane
```

新規の`adminhello.tane`はadminラベルになるため、後半のexecは拒否されます。execでは、呼び出し元だけでなく、実行先のuserドメインにも読み取り権限が必要です。adminが読めるファイルを、adminの権限でuserのメモリへコピーすることはできません。

user向けの新しい実行ファイルは、シェルをuserへ下げてから作成できます。dropは再起動まで元に戻せず、変数・入力履歴・保留中のplanも消します。

```text
drop
proc install echo user-echo.tane
file list
proc exec user-echo.tane "from saved executable"
```

表示されたPIDを新しく変数へ設定し、`proc wait $PID`で結果を読んでください。保存したuserラベルの実行ファイルは、次回の起動でadminのシェルからでもexecできます。実行先userが読めることが条件です。

新規ファイルのラベルは作成ドメインです。既存ファイルを上書きしてもラベルは保持し、installで他ドメインのラベルへ変える操作はありません。userはadminのファイルを上書きできません。

## 権限と資源

プロセスは必ずUserドメインです。Adminから起動してもAdminの権限やフレーム上限を引き継ぎません。起動する呼び出し元のSpawn権限と、実行先Userの資源上限を別々に確認します。

| 1プロセスが使うもの | 4 KiBフレーム数 |
| --- | --- |
| 独立ページテーブル | 4 |
| コード | 1 |
| データとBSS | 1 |
| userスタック | 2 |
| syscall・割り込み用kernelスタック | 4 |
| 起動時の基本合計 | 12 |
| 追加ヒープ | 0〜8 |

Userの上限は24フレーム・3タスクです。ヒープを持たず、ほかにUserのフレームを使っていなければ、同時に動かせるプロセスは最大2個です。追加ヒープもこの24フレームに含めます。1個のプロセスが1ページ以上のヒープを持つと、2個目の基本12フレームを確保できません。Userの`alloc`やデモtaskも同じ勘定を使います。カーネル全体のtask表はshellとidleを含め8件です。空きや割り当てが足りない起動は、途中で確保した資源も戻して拒否します。

終了・kill・user例外でhandleを失効させ、CPUがそのCR3とkernelスタックを離れてから、ページとスタックをゼロにして回収します。回収したタスクとフレームは、作成時に固定したUserの勘定から返します。最近の終了結果はフレームを保持せず、`frames`は0になります。

CPUはPITの100 Hzタイマーで横取りします。UserのCPU配分は30%で、他ドメインと競うときの優先順位に使います。ほかに動くtaskがなければCPUを遊ばせず、配分を超えて実行できます。これは停止時間の厳密な上限ではありません。

ファイルのMACはAdmin/Userドメイン単位です。すべてのuserプロセスはUserラベルのファイルを共有し、同じドメインに許可したファイルやプロセス出力をPID別に秘密にはしません。個々のhandleのPID制限は、他プロセスが持つtokenの借用を防ぐためのものです。

## メモリの境界

同じ仮想アドレスを、プロセスごとに違う物理ページへ対応付けます。切り替えではCR3とTSSのRSP0を更新し、syscallと割り込みはそのプロセスのkernelスタックへ入ります。

| user仮想アドレス | 配置と保護 |
| --- | --- |
| `0x40000000`〜`0x40000fff` | コード、読み取り・実行可、書き込み不可 |
| `0x40001000`〜`0x40001fff` | データとゼロ初期化BSS、読み書き可・NX |
| `0x40002000`〜`0x40003fff` | 未対応付け。スタックの下側ガード |
| `0x40004000`〜`0x40005fff` | 8 KiBのuserスタック、読み書き可・NX |
| `0x40006000`〜`0x4000ffff` | 未対応付け。スタックとヒープの間のガード |
| `0x40010000`〜`0x40017fff` | 追加ヒープ、確保した0〜8ページだけ読み書き可・NX |
| `0x40018000`以降 | userの対応付けなし。最大ヒープの直後もガード |

カーネルの同一アドレス対応はsupervisor専用で、userからコード・データ・VGA・ページテーブルへ直接アクセスできません。RXコードへの書き込み、NXのデータやスタックへのジャンプ、未対応付けページ、権限のないI/O命令などのCPU例外は、そのプロセスだけを終了します。カーネルのCPU例外は従来どおり別に扱います。

カーネルはuserのポインタを直接参照せず、現在のアドレス空間で要求範囲全体を検査してからsupervisorの物理対応を通してコピーします。範囲の加算overflow、kernel/noncanonicalアドレス、ガードまたぎ、書き込み不可ページを拒否します。0バイトの要求でも、ポインタ自体が適切な対応付けの中にある必要があります。不正な範囲の先頭部分だけをコピーしたり、ファイルの一部だけを変更したりはしません。

ユーザープロセスにはNX対応CPUが必要です。現在はCPUの拡張状態を保存するABIを持たず、x87/MMX/SSE/AVXを使えません。CR0.TSを有効化してFPU/SIMDの使用を例外にし、XSAVEとFSGSBASEも無効化しています。FS/GSベースは0で、TLSはありません。プログラムは整数処理だけで書きます。

## ヒープを増やす・返す

syscall 14のRDIに、バイト数ではなく希望する**総ページ数**0〜8を渡します。成功時は常に開始アドレス`0x40010000`を返します。起動時は0ページで、最大8ページ・32 KiBです。0ページの成功結果も同じアドレスですが、そのアドレスは未対応付けなので参照できません。

増加時は、必要なUserのフレーム枠と物理ページをすべて確保し、ゼロ初期化してから対応付けを公開します。途中で不足しても、元のヒープサイズ・データ・対応付けを保ち、途中の確保を戻します。ページ数が8を超えれば`EINVAL`、Userの上限不足なら`EACCES`（quota拒否として監査）、物理ページの不足なら`ENOMEM`です。

縮小は末尾のページを外します。外したページのTLBを無効化してから内容をゼロにし、フレームとUserの勘定を返します。残した先頭部分のデータは保ち、後で増やしたページは再びゼロから始まります。縮小した領域へのCPUアクセスはページフォルトになり、syscallへ渡せば`EFAULT`になります。終了時も追加ヒープを含めて全ページを回収します。

これはページ単位のメモリ管理で、`malloc`やRustの`GlobalAlloc`は提供しません。アプリケーションが、この領域内の配置・寿命を管理します。共有メモリ、copy-on-write、swap、任意アドレスへの対応付けはありません。

## 独自syscallとファイルhandle

RAXに番号、RDI・RSI・RDXに引数を入れ、`int 0x80`を呼びます。RAXに符号付き64ビットの結果が戻り、負の値はエラーです。ほかの汎用レジスタは保持します。0.9で加えた要求は、使わないRSI/RDXを0にする規約で、14・15・17は両方0、16・18・20はRDXを0にします。Linuxの番号・errno互換を要求するABIではありません。

| 番号 | 操作 | 引数と上限 |
| --- | --- | --- |
| 0 | exit | 終了コード。戻らない |
| 1 | stdout | 読めるuserポインタ、長さ0〜256 |
| 2 | PID | 自分のPIDを返す |
| 3 | yield | CPUを譲る |
| 4 | sleep | 0〜60000 ms |
| 5 | open | 名前ポインタ、名前長1〜47、read/write権限 |
| 6 | read | token、書けるuserポインタ、長さ0〜256 |
| 7 | append | token、読めるuserポインタ、長さ0〜256 |
| 8 | close | tokenを失効 |
| 9 | unlink | 名前ポインタと名前長 |
| 10 | ticks | PITのティック数 |
| 11〜13 | format/audit/networkの検査用要求 | Userとして拒否し、監査する |
| 14 | heap resize | 希望する総ページ数0〜8。成功時`0x40010000` |
| 15 | spawn child | 40バイトの要求への読めるuserポインタ。成功時は子のPID |
| 16 | wait child | 正の子PID、40バイトの結果への書けるuserポインタ。成功時は子のPID |
| 17 | kill child | 自分の実行中の子PID。成功時0 |
| 18 | seek file | token、位置0〜4096。成功時は位置 |
| 19 | write file | token、読めるuserポインタ、長さ0〜256。cursor位置から書く |
| 20 | truncate file | token、サイズ0〜4096。成功時はサイズ |

syscall 15の要求は、little-endianの`u64`を5つ順に並べます。構造体自体と、その中の名前・引数の全範囲を検査・コピーしてから、ファイルの読み込みやプロセス作成を始めます。

| 40バイトspawn要求の位置 | 値 |
| --- | --- |
| 0〜7 | 名前のuserポインタ |
| 8〜15 | 名前長1〜47バイト |
| 16〜23 | 引数のuserポインタ |
| 24〜31 | 引数長0〜128バイト |
| 32〜39 | 種類。0は同梱プログラム、1はTaneFS実行ファイル |

引数長0でも、そのポインタは対応付け済みのuserページ内に必要です。種類の他の値は`EINVAL`です。

| 40バイトwait結果の位置 | 値 |
| --- | --- |
| 0〜7 | 子PID |
| 8〜15 | 種類。0はexit、1はCPU例外、2はkill |
| 16〜23 | exitの符号付き64ビットコードのビット列。例外・killでは0 |
| 24〜31 | CPU例外のvector。他の場合は0 |
| 32〜39 | CPU例外のerror code。他の場合は0 |

wait結果には例外時のRIP、CR2、kernelのポインタを含めません。

ファイルhandleは1プロセス4個です。READ=1、WRITE=2、両方=3で、open後に権限を増やせません。WRITEで存在しないファイルを開くとUserとして空のファイルを作ります。既存ファイルは切り詰めません。appendは末尾へ追記し、READだけのhandleでは書けません。readのオフセットはhandleごとに持ちます。

syscall 18のseekはREADまたはWRITEを持つhandleのcursorを0〜4096へ変更します。EOFより後ろも指定でき、readはEOFで0バイトを返します。syscall 19のwriteはWRITE権限を要求し、現在のcursorから置き換え、成功時だけcursorを進めます。EOFの後ろから書いた場合の隙間をゼロにします。syscall 20のtruncateもWRITEを要求し、指定サイズへ縮小またはゼロで延長します。truncateはcursorを動かしません。0バイトのwriteは変更せず、EOFやcursorを進めません。appendは従来どおりEOFへ書くため、位置を指定したwriteとは区別します。

seek以外の内容変更では、そのhandle自身のidentityを更新し、同じ対象を固定した他のhandleをstaleにします。TaneFS v1への変更は読み取り専用として拒否します。通常のファイル変更はv2のjournalを通り、flushの前提とI/O失敗時の`EIO`は[ストレージv2](storage-v2.md)を参照してください。

tokenは正の値で、起動中に再利用せず、発行したPIDのhandle表にだけ有効です。close後のtoken、他PIDのtoken、slotを再利用した後の古いtokenは使えません。handleはファイル名に加えてslot・ラベル・サイズ・世代・チェックサム・起動中の変更epochを固定します。同じ名前を削除して作り直した場合も、古いhandleを新しい対象へ付け替えません。

appendが成功したhandle自身は、更新後のidentityを持ちます。同じファイルへの別の古いhandleはstaleになります。read/writeのたびに対象のidentityとMACを再検査します。handleの権限不足、他PID・失効tokenのread/write、古いidentityでのread/writeの拒否は、ドメインMACとは別のcapability理由で監査します。

主なエラーは`ENOMEM=-12`（物理メモリ不足）、`EFAULT=-14`（不正なuser範囲）、`EINVAL=-22`（引数）、`EROFS=-30`（旧v1ディスクへの変更）、`EACCES=-13`（権限）、`EBADF=-9`（handle）、`ESTALE=-116`（対象変更）、`ENOSPC=-28`（容量）、`EAGAIN=-11`（出力保持上限・未消費の子）、`ECHILD=-10`（wait/kill権限のある子がない）、`ENOSYS=-38`（未知の番号）です。ディスクのI/O失敗は`EIO=-5`として返り、変更中ならstorageのマウント状態を無効化します。自動ロールバックを保証するABIではありません。

ユーザープロセスによるファイル変更も、シェルの保留中planを失効させます。シェルから見えるストレージとuserのsyscallは同じ実装・変更番号・MACを使います。

## 同梱プログラムと検査用プログラム

| 名前 | 引数 | 動作 |
| --- | --- | --- |
| `hello` | なし | 挨拶とPIDを出して終了 |
| `echo` | 文字列 | 引数と改行を出して終了 |
| `sleep` | ミリ秒 | sleepしてから終了 |
| `busy` | なし | ループし続ける。timerによる横取りとkillを試す |
| `isolate` | 整数 | データのゼロ初期化と、横取りをまたぐ専用ページの値を確かめる |
| `heap` | `basic`、`limit`、`hold N`、`quota` | ヒープの増減・ゼロ初期化・全範囲コピー・上限・使用量を試す |
| `heap` | `freed`、`nx`、`guard` | 解放済みページ・NXヒープ・ヒープ前のガードへのアクセスで例外を起こす |
| `control` | `basic`、`live`、`exit`、`fault`、`kill` | 子の起動・待機・終了37・例外・killの結果を受け取る |
| `control` | `file NAME`、`deniedfile NAME` | TaneFSからの子の起動・Admin実行ファイルの拒否を試す |
| `control` | `foreign PID` | 実行中の無関係なUserのPIDへのwait/kill拒否を試す |
| `control` | `validate`、`quota`、`pending`、`orphan` | 要求の検査・親子の権限・資源不足と再試行・未消費の終了結果・親の終了を試す |
| `files` | ファイル名 | handleで追記・読み出しを試す |
| `files` | `position` | seek・位置からのwrite・truncate、ゼロで埋める隙間、cursorとstale handleを検査 |
| `files` | `badptr`、`rights`、`stale`、`hold`、`steal TOKEN` | ポインタ・権限・対象identity・他PID tokenの検査 |
| `probe` | `badptr`、`denied` | syscallの範囲や拒否される操作の検査 |
| `probe` | `kernel-read`、`code-write`、`nx`、`guard`、`cli`、`hlt`、`in`、`out`、`sse`、`x87`など | 意図的にuser側のCPU例外を起こす |

probeの結果は`proc wait`で読み、その後もシェルを操作できます。ring 0側の例外検査は従来の`fault`で、kernelの停止を伴うものがあるため、user probeとは別です。検査モード全体は[users/README.md](../users/README.md)にあります。

## 自分のRustプログラムを組み直す

必要なツールはカーネルと同じRust 1.90.0の`x86_64-unknown-none`、GNU binutils、Python 3です。Cargoや外部crateは使いません。

```sh
python3 users/build.py
# カーネルへ同梱する分も更新する:
python3 build.py
sh run.sh
```

出力は`build/users/PROGRAM.tane`です。開発時にはELFを中間生成物として使いますが、カーネルのloaderが読むのは独自のTane形式です。

`users/hello.rs`が最小の見本です。`#![no_std]`・`#![no_main]`、独自の`_start`と`users/common.rs`のsyscallラッパーを使います。開始時には引数ポインタがRDI、長さがRSIに入り、RSPは関数入口の16バイト境界規約に合わせます。`_start`はreturnせず、exit syscallで終わります。

新しい名前のプログラムを追加する場合は、`users/build.py`のPROGRAMSと`src/user_images.rs`の同梱一覧に登録してから、カーネルを組み直します。現時点でシェルから実行ファイルを保存する導入経路は、同梱プログラムの`proc install`です。汎用のバイナリアップロードやホストファイルへのアクセス機能はありません。

Tane形式は32バイトのlittle-endianヘッダー、コード、初期値付きデータを順に並べます。magicは`TANEEXE\0`、版は1、flagsと予約領域は0です。ファイル全体はヘッダー込みで4096バイト以内、コードは空でなく1ページ以内、データ+BSSは1ページ以内、入口はコードの中に限ります。余分な末尾データ、対応外の版・flags、不正な長さは実行前に拒否します。再配置、動的リンク、ELF実行はありません。ヒープは実行ファイルのセクションではなく、起動後にsyscallで確保する匿名ページです。詳細は[形式とABI](../users/README.md)を参照してください。

## 検証と記録

```sh
python3 build.py
sh tests/host.sh
python3 tests/smoke.py
python3 tests/network.py
python3 tests/shell.py
python3 tests/process.py
python3 tests/advanced.py
```

hostテストは実行ファイル形式、user範囲・ページ配置、handleの権限・PID・再利用、syscallの数値上限を確認します。`tests/process.py`は実際にBIOSから起動したQEMU上で、CPL3、独立CR3、timerによる横取り、CPU例外、syscallコピー、handleとファイルMAC、Userの資源勘定、終了時の回収、保存した実行ファイルの再起動後の実行を確認します。`tests/advanced.py`は有界ヒープ、pause/resume、親子のspawn/wait/kill、未消費の終了結果、孤児、seek/write/truncateと再起動後の保存を確かめます。結果は`build/process-summary.txt`・`build/process-serial.txt`と、`build/advanced-summary.txt`・`build/advanced-serial.txt`へ残します。
