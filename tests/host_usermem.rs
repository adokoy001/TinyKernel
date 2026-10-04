//! Exercise the production address-space implementation with aligned physical
//! frames, sparse allocation, deterministic exhaustion and a recording TLB.
#![allow(dead_code)]
#[path="../src/executable.rs"] mod executable;
#[path="../src/usermem.rs"] mod usermem;

use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Debug)]
enum Event {
    Alloc(u64, usize),
    Free(u64, usize),
    Invalidate { root: u64, start: u64, entries: Vec<u64> },
}

mod paging {
    use super::{Event, FRAMES};
    pub fn nx_enabled()->bool { true }
    pub fn kernel_pd()->u64 { 0x123000 }
    pub unsafe fn invalidate_user_range(root:u64,start:u64,pages:usize) {
        let first=((start-super::usermem::CODE_BASE)/4096) as usize;
        let pt=(root+3*4096) as *const u64;
        let entries=(0..pages).map(|page|pt.add(first+page).read()).collect();
        FRAMES.with(|state|state.borrow_mut().events.push(Event::Invalidate{root,start,entries}));
    }
}

const POOL_PAGES:usize=64;
struct Frames {
    base:u64,
    used:[bool;POOL_PAGES],
    allocations:BTreeMap<u64,usize>,
    fail_after:Option<usize>,
    events:Vec<Event>,
}
impl Frames {
    fn new()->Self {
        let layout=std::alloc::Layout::from_size_align(POOL_PAGES*4096,4096).unwrap();
        let base=unsafe{std::alloc::alloc(layout)};
        assert!(!base.is_null());
        unsafe{base.write_bytes(0xa5,POOL_PAGES*4096)};
        Self{base:base as u64,used:[false;POOL_PAGES],allocations:BTreeMap::new(),fail_after:None,events:Vec::new()}
    }
    fn allocate_contiguous(&mut self,count:usize)->Option<u64> {
        if let Some(remaining)=self.fail_after.as_mut() {
            if *remaining==0 {return None;}
            *remaining-=1;
        }
        // Single heap pages deliberately occupy nonadjacent physical slots.
        // Cross-page user copies must consult each page's owned frame.
        let sparse=(count==1).then(||(0..POOL_PAGES).map(|n|(20+2*n)%POOL_PAGES).find(|index|!self.used[*index])).flatten();
        let first=sparse.or_else(||(0..=POOL_PAGES-count).find(|first|self.used[*first..*first+count].iter().all(|v|!*v)))?;
        self.used[first..first+count].fill(true);
        let address=self.base+first as u64*4096;
        assert!(self.allocations.insert(address,count).is_none());
        // Pretend firmware and previous owners left nonzero contents. Every
        // production allocation must clear these before user publication.
        unsafe{(address as *mut u8).write_bytes(0xa5,count*4096)};
        self.events.push(Event::Alloc(address,count));
        Some(address)
    }
    fn free_contiguous(&mut self,base:u64,count:usize)->Result<(),()> {
        assert_eq!(self.allocations.remove(&base),Some(count),"free must match an owned allocation");
        unsafe {
            let bytes=std::slice::from_raw_parts(base as *const u8,count*4096);
            assert!(bytes.iter().all(|v|*v==0),"entire allocation must be wiped before free");
        }
        let first=((base-self.base)/4096) as usize;
        self.used[first..first+count].fill(false);
        self.events.push(Event::Free(base,count));
        Ok(())
    }
}
impl Drop for Frames {
    fn drop(&mut self) {
        assert!(self.allocations.is_empty(),"all process frames must be reclaimed");
        unsafe{std::alloc::dealloc(self.base as *mut u8,std::alloc::Layout::from_size_align(POOL_PAGES*4096,4096).unwrap())}
    }
}
thread_local! {static FRAMES:RefCell<Frames>=RefCell::new(Frames::new());}
fn with_frames<R>(f:impl FnOnce(&mut Frames)->R)->R {FRAMES.with(|state|f(&mut state.borrow_mut()))}
fn exe()->Vec<u8> {
    let mut out=vec![0;37];out[..8].copy_from_slice(executable::MAGIC);
    out[8..10].copy_from_slice(&1u16.to_le_bytes());out[12..14].copy_from_slice(&32u16.to_le_bytes());
    for (at,val) in [(16,3u32),(20,2),(24,10),(28,1)] {out[at..at+4].copy_from_slice(&val.to_le_bytes());}
    out[32..].copy_from_slice(&[0x90,0x90,0xc3,4,5]);out
}
fn entry(space:&usermem::AddressSpace,index:usize)->u64 {
    unsafe{((space.root()+3*4096) as *const u64).add(index).read()}
}
fn memory(space:&usermem::AddressSpace,pointer:u64,len:usize)->Vec<u8> {
    let mut bytes=vec![0;len];space.copy_from_user(pointer,&mut bytes).unwrap();bytes
}
fn main() {
    use usermem::{AddressSpace,HeapError,CODE_BASE,DATA_BASE,STACK_BASE,STACK_TOP,HEAP_BASE,HEAP_MAX_PAGES,HEAP_LIMIT};
    let bytes=exe();let image=executable::Image::parse(&bytes).unwrap();
    let mut a=AddressSpace::create(&image,b"hello").unwrap();
    let mut b=AddressSpace::create(&image,b"world").unwrap();
    assert_ne!(a.root(),b.root());assert_eq!(a.entry(),CODE_BASE+1);
    assert_eq!(a.initial_rsp(),STACK_TOP-8);assert_eq!(a.argument(),(STACK_BASE,5));
    assert_eq!(a.heap_pages(),0);assert_eq!(a.owned_frames(),8);
    unsafe {
        let flags=|offset:usize,index:usize| ->u64 {*((a.root() as usize+offset*4096) as *const u64).add(index)};
        assert_eq!(flags(0,0)&7,7);assert_eq!(flags(1,0),0x123003);
        assert_eq!(flags(1,1)&7,7);assert_eq!(flags(2,0)&7,7);
        assert_eq!(flags(3,0)&7,5);assert_eq!(flags(3,0)>>63,0);
        for index in [1,4,5] {assert_eq!(flags(3,index)&7,7);assert_eq!(flags(3,index)>>63,1);}
        for index in [2,3,6,16,23,24,511] {assert_eq!(flags(3,index),0);}
    }
    assert_eq!(&memory(&a,DATA_BASE,14)[..2],&[4,5]);
    assert!(memory(&a,DATA_BASE+2,12).iter().all(|v|*v==0));
    assert_eq!(memory(&a,STACK_BASE,5),b"hello");assert_eq!(memory(&b,STACK_BASE,5),b"world");
    a.copy_to_user(DATA_BASE,&[7]).unwrap();assert_eq!(memory(&b,DATA_BASE,1),[4]);
    assert!(a.copy_to_user(CODE_BASE,&[0]).is_err());
    assert!(a.copy_to_user(DATA_BASE+4095,&[9,9]).is_err());assert_eq!(memory(&a,DATA_BASE+4095,1),[0]);
    let mut output=[0xa5;2];assert!(a.copy_from_user(DATA_BASE+4095,&mut output).is_err());assert_eq!(output,[0xa5;2]);
    a.copy_from_user(DATA_BASE-1,&mut output).unwrap();assert_eq!(output,[0,7]);
    a.copy_to_user(STACK_BASE+4095,&[3,8]).unwrap();assert_eq!(memory(&a,STACK_BASE+4095,2),[3,8]);
    println!("PASS: base page permissions, process isolation, zero BSS/arguments and full-span copies");

    assert!(a.validate_read(HEAP_BASE,0).is_err());
    a.resize_heap(3).unwrap();b.resize_heap(2).unwrap();
    assert_eq!(a.heap_pages(),3);assert_eq!(a.owned_frames(),11);
    for index in 16..19 {assert_eq!(entry(&a,index)&7,7);assert_eq!(entry(&a,index)>>63,1);}
    assert_eq!(entry(&a,19),0);assert_eq!(entry(&a,24),0);
    let mask=0x000f_ffff_ffff_f000;
    assert_ne!((entry(&a,16)&mask)+4096,entry(&a,17)&mask,"test uses noncontiguous physical heap pages");
    let a_frames=(16..19).map(|index|entry(&a,index)&mask).collect::<Vec<_>>();
    let b_frames=(16..18).map(|index|entry(&b,index)&mask).collect::<Vec<_>>();
    assert!(a_frames.iter().all(|frame|!b_frames.contains(frame)));
    assert!(memory(&a,HEAP_BASE,3*4096).iter().all(|v|*v==0));
    let payload=(0..5000).map(|n|(n%251) as u8).collect::<Vec<_>>();
    a.copy_to_user(HEAP_BASE+4090,&payload).unwrap();assert_eq!(memory(&a,HEAP_BASE+4090,payload.len()),payload);
    assert!(memory(&b,HEAP_BASE,2*4096).iter().all(|v|*v==0));
    println!("PASS: sparse RW/NX heap mappings, noncontiguous cross-page copies and process isolation");

    let top=HEAP_BASE+3*4096;
    let tail=memory(&a,top-1,1);
    assert!(a.copy_to_user(top-1,&[9,9]).is_err());assert_eq!(memory(&a,top-1,1),tail);
    let mut destination=[0x55;2];assert!(a.copy_from_user(top-1,&mut destination).is_err());assert_eq!(destination,[0x55;2]);
    assert!(a.validate_read(top,0).is_err());assert!(a.validate_write(HEAP_LIMIT,0).is_err());
    let old_entries=(16..24).map(|index|entry(&a,index)).collect::<Vec<_>>();
    assert_eq!(a.resize_heap(HEAP_MAX_PAGES+1),Err(HeapError::TooLarge));
    assert_eq!(a.heap_pages(),3);assert_eq!(old_entries,(16..24).map(|index|entry(&a,index)).collect::<Vec<_>>());
    println!("PASS: unmapped heap end, permanent guard, oversize rejection and no partial boundary writes");

    let before=with_frames(|frames|frames.allocations.clone());
    let before_events=with_frames(|frames|frames.events.len());
    with_frames(|frames|frames.fail_after=Some(2));
    assert_eq!(a.resize_heap(7),Err(HeapError::OutOfMemory));
    with_frames(|frames|frames.fail_after=None);
    assert_eq!(a.heap_pages(),3);assert_eq!(a.owned_frames(),11);
    assert_eq!(with_frames(|frames|frames.allocations.clone()),before);
    assert_eq!(old_entries,(16..24).map(|index|entry(&a,index)).collect::<Vec<_>>());
    assert_eq!(memory(&a,HEAP_BASE+4090,payload.len()),payload);
    with_frames(|frames|{
        let events=&frames.events[before_events..];
        assert_eq!(events.len(),4);
        assert!(matches!(events[0],Event::Alloc(_,1)));assert!(matches!(events[1],Event::Alloc(_,1)));
        assert!(matches!(events[2],Event::Free(_,1)));assert!(matches!(events[3],Event::Free(_,1)));
    });
    println!("PASS: partial growth exhaustion rolls back every allocation without publishing or changing old bytes");

    let before_shrink=with_frames(|frames|frames.events.len());
    a.resize_heap(1).unwrap();assert_eq!(a.heap_pages(),1);assert_eq!(a.owned_frames(),9);
    assert_eq!(entry(&a,17),0);assert_eq!(entry(&a,18),0);
    assert!(a.validate_read(HEAP_BASE+4096,0).is_err());
    with_frames(|frames|{
        let events=&frames.events[before_shrink..];
        assert_eq!(events.len(),3);
        match &events[0] {
            Event::Invalidate{root,start,entries}=>{assert_eq!(*root,a.root());assert_eq!(*start,HEAP_BASE+4096);assert_eq!(entries,&[0,0]);}
            other=>panic!("mapping invalidation must precede free: {other:?}"),
        }
        assert!(matches!(events[1],Event::Free(_,1)));assert!(matches!(events[2],Event::Free(_,1)));
    });
    let retained=memory(&a,HEAP_BASE,4096);
    a.resize_heap(3).unwrap();assert_eq!(memory(&a,HEAP_BASE,4096),retained);
    assert!(memory(&a,HEAP_BASE+4096,8192).iter().all(|v|*v==0));
    println!("PASS: shrink clears PTEs and invalidates before reuse; regrown pages are zero while retained bytes survive");

    a.resize_heap(8).unwrap();assert_eq!(a.owned_frames(),16);
    assert_eq!(entry(&a,24),0);assert!(a.validate_read(HEAP_LIMIT,1).is_err());
    a.copy_to_user(HEAP_LIMIT-1,&[0xe7]).unwrap();assert_eq!(memory(&a,HEAP_LIMIT-1,1),[0xe7]);
    let before_noop=with_frames(|frames|frames.events.len());a.resize_heap(8).unwrap();
    assert_eq!(with_frames(|frames|frames.events.len()),before_noop);
    a.resize_heap(0).unwrap();assert_eq!(a.owned_frames(),8);
    assert!(a.validate_write(HEAP_BASE,0).is_err());
    for index in 16..25 {assert_eq!(entry(&a,index),0);}
    println!("PASS: full heap, maximum-end guard, idempotent resize and complete shrink-to-zero");

    a.resize_heap(4).unwrap();a.copy_to_user(HEAP_BASE,&[0x99;4096]).unwrap();
    a.destroy();b.destroy();assert!(with_frames(|frames|frames.allocations.is_empty()));
    println!("PASS: complete address-space teardown wipes and reclaims base pages and all live heap pages");
}
