//! Exercise the production address-space constructor and copies with an
//! aligned host allocator; hardware CR3 activation is left to QEMU tests.
#![allow(dead_code)]
#[path="../src/executable.rs"] mod executable;
#[path="../src/usermem.rs"] mod usermem;
mod paging {
    pub fn nx_enabled()->bool { true }
    pub fn kernel_pd()->u64 { 0x123000 }
}
struct Frames;
impl Frames {
    fn allocate_contiguous(&mut self, count:usize)->Option<u64> {
        unsafe {
            let p=std::alloc::alloc(std::alloc::Layout::from_size_align(count*4096,4096).unwrap());
            if p.is_null() { return None; }
            p.write_bytes(0xa5,count*4096);
            Some(p as u64)
        }
    }
    fn free_contiguous(&mut self, base:u64,count:usize)->Result<(),()> {
        unsafe {
            let bytes=std::slice::from_raw_parts(base as *const u8,count*4096);
            assert!(bytes.iter().all(|v|*v==0),"entire allocation must be wiped before free");
            std::alloc::dealloc(base as *mut u8,std::alloc::Layout::from_size_align(count*4096,4096).unwrap());
        }
        Ok(())
    }
}
fn with_frames<R>(f:impl FnOnce(&mut Frames)->R)->R { f(&mut Frames) }
fn exe()->Vec<u8> {
    let mut out=vec![0;37]; out[..8].copy_from_slice(executable::MAGIC);
    out[8..10].copy_from_slice(&1u16.to_le_bytes()); out[12..14].copy_from_slice(&32u16.to_le_bytes());
    for (at,val) in [(16,3u32),(20,2),(24,10),(28,1)] { out[at..at+4].copy_from_slice(&val.to_le_bytes()); }
    out[32..].copy_from_slice(&[0x90,0x90,0xc3,4,5]); out
}
fn main() {
    use usermem::{AddressSpace,CODE_BASE,DATA_BASE,STACK_BASE,STACK_TOP};
    let bytes=exe(); let image=executable::Image::parse(&bytes).unwrap();
    let a=AddressSpace::create(&image,b"hello").unwrap();
    let b=AddressSpace::create(&image,b"world").unwrap();
    assert_ne!(a.root(),b.root()); assert_eq!(a.entry(),CODE_BASE+1);
    assert_eq!(a.initial_rsp(),STACK_TOP-8); assert_eq!(a.argument(),(STACK_BASE,5));
    unsafe {
        let flags=|offset:usize,index:usize| ->u64 { *((a.root() as usize+offset*4096) as *const u64).add(index) };
        assert_eq!(flags(0,0)&7,7); assert_eq!(flags(1,0),0x123003);
        assert_eq!(flags(1,1)&7,7); assert_eq!(flags(2,0)&7,7);
        assert_eq!(flags(3,0)&7,5); assert_eq!(flags(3,0)>>63,0);
        for index in [1,4,5] { assert_eq!(flags(3,index)&7,7); assert_eq!(flags(3,index)>>63,1); }
        for index in [2,3,6,511] { assert_eq!(flags(3,index),0); }
    }
    let mut data=[0;14]; a.copy_from_user(DATA_BASE,&mut data).unwrap(); assert_eq!(data[..2],[4,5]); assert!(data[2..].iter().all(|v|*v==0));
    let mut args=[0;5]; a.copy_from_user(STACK_BASE,&mut args).unwrap(); assert_eq!(&args,b"hello");
    b.copy_from_user(STACK_BASE,&mut args).unwrap(); assert_eq!(&args,b"world");
    a.copy_to_user(DATA_BASE,&[7]).unwrap(); let mut first=[0]; b.copy_from_user(DATA_BASE,&mut first).unwrap(); assert_eq!(first,[4]);
    assert!(a.copy_to_user(CODE_BASE,&[0]).is_err());
    assert!(a.copy_to_user(DATA_BASE+4095,&[9,9]).is_err());
    a.copy_from_user(DATA_BASE+4095,&mut first).unwrap(); assert_eq!(first,[0]);
    let mut output=[0xa5;2]; assert!(a.copy_from_user(DATA_BASE+4095,&mut output).is_err()); assert_eq!(output,[0xa5;2]);
    a.copy_from_user(DATA_BASE-1,&mut output).unwrap(); assert_eq!(output,[0,7]);
    a.copy_to_user(STACK_BASE+4095,&[3,8]).unwrap(); a.copy_from_user(STACK_BASE+4095,&mut output).unwrap(); assert_eq!(output,[3,8]);
    a.destroy(); b.destroy(); println!("PASS: page flags, page isolation, zeroed BSS/arguments, full-span atomic validation, cross-page copies, complete teardown wipe");
}
