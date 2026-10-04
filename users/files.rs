#![no_std]
#![no_main]
mod common;
fn open(path:&[u8],rights:u64)->i64 { common::syscall(5,path.as_ptr() as u64,path.len() as u64,rights) }
#[no_mangle] #[link_section=".text.entry"]
pub unsafe extern "C" fn _start(ptr:*const u8,len:usize)->! {
    let arg=common::args(ptr,len);
    if let Some(token)=arg.strip_prefix(b"steal ") {
        let handle=common::number(token,0); let mut byte=0u8;
        if common::syscall(6,handle,(&raw mut byte) as u64,1)!=-9 {
            common::print(b"files: foreign handle accepted\n"); common::exit(6);
        }
        common::print(b"files: foreign handle rejected\n"); common::exit(0);
    }
    if arg==b"stale" {
        let writer=open(b"ring3-stale",3); let reader=open(b"ring3-stale",1);
        if writer<0 || reader<0 { common::print(b"files: stale setup failed\n"); common::exit(7); }
        let mut byte=0u8;
        let written=common::syscall(7,writer as u64,b"x".as_ptr() as u64,1);
        let stale=common::syscall(6,reader as u64,(&raw mut byte) as u64,1);
        common::syscall(8,writer as u64,0,0); common::syscall(8,reader as u64,0,0);
        let replacement=open(b"ring3-stale",1);
        let replay=common::syscall(6,writer as u64,(&raw mut byte) as u64,1);
        common::syscall(8,replacement as u64,0,0);
        if written!=1 || stale!=-116 || replay!=-9 { common::print(b"files: stale/replay failure\n"); common::exit(8); }
        common::print(b"files: stale generation and reused handle rejected\n"); common::exit(0);
    }
    if arg==b"hold" {
        let handle=open(b"ring3-held",3);
        if handle<0 { common::print(b"files: hold setup failed\n"); common::exit(9); }
        common::print(b"files: handle="); common::print_number(handle as u64); common::print(b"\n");
        common::sleep(5000); common::syscall(8,handle as u64,0,0); common::exit(0);
    }
    let path=if arg.is_empty() { &b"ring3-demo"[..] } else if arg==b"badptr" || arg==b"rights" { &b"ring3-cap"[..] } else { arg };
    let cap=open(path,3);
    if cap<0 { common::print(b"files: open denied/error\n"); common::exit(1); }
    if arg==b"badptr" {
        let result=common::syscall(7,cap as u64,0x10000,16);
        common::syscall(8,cap as u64,0,0);
        if result!=-14 { common::print(b"files: invalid pointer accepted\n"); common::exit(2); }
        common::print(b"files: bad pointer rejected without write\n"); common::exit(0);
    }
    if arg==b"rights" {
        common::syscall(8,cap as u64,0,0);
        let read=open(path,1);
        let result=common::syscall(7,read as u64,b"NO".as_ptr() as u64,2);
        common::syscall(8,read as u64,0,0);
        let stale=common::syscall(6,read as u64,0x40001000,1);
        if result!=-13 || stale!=-9 { common::print(b"files: capability failure\n"); common::exit(3); }
        common::print(b"files: write right and closed handle rejected\n"); common::exit(0);
    }
    let text=b"written by Rust ring3\n";
    if common::syscall(7,cap as u64,text.as_ptr() as u64,text.len() as u64)!=text.len() as i64 {
        common::print(b"files: write failed\n"); common::exit(4);
    }
    common::syscall(8,cap as u64,0,0);
    let read=open(path,1); let mut bytes=[0u8;128];
    let n=common::syscall(6,read as u64,bytes.as_mut_ptr() as u64,bytes.len() as u64);
    common::syscall(8,read as u64,0,0);
    if n<0 || n as usize>bytes.len() { common::print(b"files: read failed\n"); common::exit(5); }
    common::print(b"files: "); common::print(&bytes[..n as usize]); common::exit(0)
}
