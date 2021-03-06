// Copyright 2016 Philipp Oppermann. See the README.md
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use core::mem;
use core::ops::Deref;
use core::ptr::Unique;
use core::slice;
use {BROADCAST_TLB_SHOOTDOWN_FUNC, VirtualAddress, PhysicalAddress, get_frame_allocator_ref, FrameRange, Page, Frame, FrameAllocator, AllocatedPages}; 
use paging::{PageRange, get_current_p4};
use paging::table::{P4, Table, Level4};
use kernel_config::memory::{ENTRIES_PER_PAGE_TABLE, PAGE_SIZE};
use irq_safety::MutexIrqSafe;
use super::{EntryFlags, tlb_flush_virt_addr};
use zerocopy::FromBytes;

pub struct Mapper {
    p4: Unique<Table<Level4>>,
    /// The Frame contaning the top-level P4 page table.
    pub target_p4: Frame,
}

impl Mapper {
    pub fn from_current() -> Mapper {
        Self::with_p4_frame(get_current_p4())
    }

    pub fn with_p4_frame(p4: Frame) -> Mapper {
        Mapper { 
            p4: Unique::new(P4).unwrap(), // cannot panic because we know the P4 value is valid
            target_p4: p4,
        }
    }

    pub fn p4(&self) -> &Table<Level4> {
        unsafe { self.p4.as_ref() }
    }

    pub fn p4_mut(&mut self) -> &mut Table<Level4> {
        unsafe { self.p4.as_mut() }
    }

    /// Dumps all page table entries at all four levels for the given `VirtualAddress`, 
    /// and also shows their `EntryFlags`.
    /// 
    /// Useful for debugging page faults. 
    pub fn dump_pte(&self, virtual_address: VirtualAddress) {
        let page = Page::containing_address(virtual_address);
        let p4 = self.p4();
        let p3 = p4.next_table(page.p4_index());
        let p2 = p3.and_then(|p3| p3.next_table(page.p3_index()));
        let p1 = p2.and_then(|p2| p2.next_table(page.p2_index()));
        if let Some(_pte) = p1.map(|p1| &p1[page.p1_index()]) {
            debug!("VirtualAddress: {:#X}:
                    P4 entry:        {:#X}   ({:?})
                    P3 entry:        {:#X}   ({:?})
                    P2 entry:        {:#X}   ({:?})
                    P1 entry: (PTE)  {:#X}   ({:?})",
                virtual_address, 
                &p4[page.p4_index()].value(), 
                &p4[page.p4_index()].flags(),
                p3.map(|p3| &p3[page.p3_index()]).map(|p3_entry| p3_entry.value()).unwrap_or(0x0), 
                p3.map(|p3| &p3[page.p3_index()]).map(|p3_entry| p3_entry.flags()),
                p2.map(|p2| &p2[page.p2_index()]).map(|p2_entry| p2_entry.value()).unwrap_or(0x0), 
                p2.map(|p2| &p2[page.p2_index()]).map(|p2_entry| p2_entry.flags()),
                p1.map(|p1| &p1[page.p1_index()]).map(|p1_entry| p1_entry.value()).unwrap_or(0x0),  // _pet.value()
                p1.map(|p1| &p1[page.p1_index()]).map(|p1_entry| p1_entry.flags()),                 // _pte.flags()
            );
        }
        else {
            debug!("Error: couldn't get PTE entry for vaddr: {:#X}. Has it been mapped?", virtual_address);
        }
    }

    /// Translates a `VirtualAddress` to a `PhysicalAddress` by walking the page tables.
    pub fn translate(&self, virtual_address: VirtualAddress) -> Option<PhysicalAddress> {
        // get the frame number of the page containing the given virtual address,
        // and then the corresponding physical address is that page frame number * page size + offset
        self.translate_page(Page::containing_address(virtual_address))
            .map(|frame| frame.start_address() + virtual_address.page_offset())
    }

    /// Translates a virtual memory `Page` to a physical memory `Frame` by walking the page tables.
    pub fn translate_page(&self, page: Page) -> Option<Frame> {
        let p3 = self.p4().next_table(page.p4_index());

        let huge_page = || {
            p3.and_then(|p3| {
                let p3_entry = &p3[page.p3_index()];
                // 1GiB page?
                if let Some(start_frame) = p3_entry.pointed_frame() {
                    if p3_entry.flags().is_huge() {
                        // address must be 1GiB aligned
                        assert!(start_frame.number % (ENTRIES_PER_PAGE_TABLE * ENTRIES_PER_PAGE_TABLE) == 0);
                        return Some(Frame {
                            number: start_frame.number + page.p2_index() * ENTRIES_PER_PAGE_TABLE + page.p1_index(),
                        });
                    }
                }
                if let Some(p2) = p3.next_table(page.p3_index()) {
                    let p2_entry = &p2[page.p2_index()];
                    // 2MiB page?
                    if let Some(start_frame) = p2_entry.pointed_frame() {
                        if p2_entry.flags().is_huge() {
                            // address must be 2MiB aligned
                            assert!(start_frame.number % ENTRIES_PER_PAGE_TABLE == 0);
                            return Some(Frame { number: start_frame.number + page.p1_index() });
                        }
                    }
                }
                None
            })
        };

        p3.and_then(|p3| p3.next_table(page.p3_index()))
            .and_then(|p2| p2.next_table(page.p2_index()))
            .and_then(|p1| p1[page.p1_index()].pointed_frame())
            .or_else(huge_page)
    }


    /// the internal function that actually does the mapping, if frames were NOT provided.
    fn internal_map<A>(&mut self, pages: PageRange, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        // P4, P3, and P2 entries should never set NO_EXECUTE, only the lowest-level P1 entry should. 
        let mut top_level_flags = flags.clone();
        top_level_flags.set(EntryFlags::NO_EXECUTE, false);
        // top_level_flags.set(EntryFlags::WRITABLE, true); // is the same true for the WRITABLE bit?

        for page in pages.clone() {
            let frame = allocator.allocate_frame().ok_or("Mapper::internal_map(): couldn't allocate new frame, out of memory!")?;

            let p3 = self.p4_mut().next_table_create(page.p4_index(), top_level_flags, allocator);
            let p2 = p3.next_table_create(page.p3_index(), top_level_flags, allocator);
            let p1 = p2.next_table_create(page.p2_index(), top_level_flags, allocator);

            if !p1[page.p1_index()].is_unused() {
                error!("Mapper::internal_map(): page {:#x} -> frame {:#X}, page was already in use!", page.start_address(), frame.start_address());
                return Err("page was already in use");
            } 

            p1[page.p1_index()].set(frame, flags | EntryFlags::PRESENT);
        }

        Ok(MappedPages {
            page_table_p4: self.target_p4.clone(),
            pages: MaybeAllocatedPages::NotAllocated(pages),
            flags: flags,
        })
    }

    /// the internal function that actually does all of the mapping from pages to frames.
    fn internal_map_to<A>(&mut self, pages: PageRange, frames: FrameRange, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        // P4, P3, and P2 entries should never set NO_EXECUTE, only the lowest-level P1 entry should. 
        let mut top_level_flags = flags.clone();
        top_level_flags.set(EntryFlags::NO_EXECUTE, false);
        // top_level_flags.set(EntryFlags::WRITABLE, true); // is the same true for the WRITABLE bit?

        let pages_count = pages.size_in_pages();
        let frames_count = frames.size_in_frames();
        if pages_count != frames_count {
            error!("map_to_internal(): page count {} must equal frame count {}!", pages_count, frames_count);
            return Err("map_to_internal(): page count must equal frame count");
        }
            

        // iterate over pages and frames in lockstep
        for (page, frame) in pages.clone().into_iter().zip(frames) {

            let p3 = self.p4_mut().next_table_create(page.p4_index(), top_level_flags, allocator);
            let p2 = p3.next_table_create(page.p3_index(), top_level_flags, allocator);
            let p1 = p2.next_table_create(page.p2_index(), top_level_flags, allocator);

            if !p1[page.p1_index()].is_unused() {
                error!("map_to() page {:#x} -> frame {:#X}, page was already in use!", page.start_address(), frame.start_address());
                return Err("page was already in use");
            } 

            p1[page.p1_index()].set(frame, flags | EntryFlags::PRESENT);
        }

        Ok(MappedPages {
            page_table_p4: self.target_p4.clone(),
            pages: MaybeAllocatedPages::NotAllocated(pages),
            flags: flags,
        })
    }


    /// creates a mapping for a specific page -> specific frame
    pub fn map_to<A>(&mut self, page: Page, frame: Frame, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        self.internal_map_to(PageRange::new(page, page), FrameRange::new(frame.clone(), frame), flags, allocator)
    }

    /// maps the given Page to a randomly selected (newly allocated) Frame
    pub fn map<A>(&mut self, page: Page, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        self.internal_map(PageRange::new(page, page), flags, allocator)
    }

    /// maps the given `Page`s to a randomly selected (newly allocated) Frame
    pub fn map_pages<A>(&mut self, pages: PageRange, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        self.internal_map(pages, flags, allocator)
    }


    /// maps the given contiguous range of Frames `frame_range` to contiguous `Page`s starting at `start_page`
    pub fn map_frames<A>(&mut self, frames: FrameRange, start_page: Page, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        let end_page = start_page - 1 + frames.size_in_frames(); // -1 because it's an inclusive range
        self.internal_map_to(PageRange::new(start_page, end_page), frames, flags, allocator)
    }


    /// maps the given `AllocatedPages` to the given actual frames.
    /// Consumes the given `AllocatedPages` and returns a `MappedPages` object which contains that `AllocatedPages` object.
    pub fn map_allocated_pages_to<A>(&mut self, allocated_pages: AllocatedPages, frames: FrameRange, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        self.internal_map_to(allocated_pages.pages.clone(), frames, flags, allocator).map(|mut mp| {
            mp.pages = MaybeAllocatedPages::Allocated(allocated_pages);
            mp
        })
    }


    /// maps the given `AllocatedPages` to randomly chosen (allocated) frames
    /// Consumes the given `AllocatedPages` and returns a `MappedPages` object which contains that `AllocatedPages` object.
    pub fn map_allocated_pages<A>(&mut self, allocated_pages: AllocatedPages, flags: EntryFlags, allocator: &mut A)
        -> Result<MappedPages, &'static str>
        where A: FrameAllocator
    {
        self.internal_map(allocated_pages.pages.clone(), flags, allocator).map(|mut mp| {
            mp.pages = MaybeAllocatedPages::Allocated(allocated_pages);
            mp
        })
    }
}


/// An instance of `MappedPages` may be created from virtual pages
/// that were either obtained from the virtual page allocator
/// or that were manually created from a predetermined virtual address and passed in. 
/// 
/// The latter option is only used for early initialization procedures 
/// before the virtual page allocator is available,
/// and should eventually be phased out when the virtual page allocator 
/// supports the ability to specify which pages we want to be allocated.
#[derive(Debug)]
enum MaybeAllocatedPages {
    /// The page range was obtained from the virtual page allocator.
    Allocated(AllocatedPages),
    /// The page range was manually created independently from the virtual page allocator. 
    /// The pages will not be deallocated once the owner (a `MappedPages` instnace) of this object is dropped. 
    NotAllocated(PageRange),
}
impl MaybeAllocatedPages {
    fn is_allocated(&self) -> bool {
        match self {
            Self::Allocated(_) => true,
            Self::NotAllocated(_) => false,
        }
    }
}
impl Deref for MaybeAllocatedPages {
    type Target = PageRange;
    fn deref(&self) -> &PageRange {
        match self {
            Self::Allocated(ap) => ap.deref(),
            Self::NotAllocated(pr) => pr,
        }
    }
}


/// Represents a contiguous range of virtual memory pages that are currently mapped. 
/// A `MappedPages` object can only have a single range of contiguous pages, not multiple disjoint ranges.
/// This does not guarantee that its pages are mapped to frames that are contiguous in physical memory.
/// 
/// This object also represents ownership of those pages; if this object falls out of scope,
/// it will be dropped, and the pages will be unmapped, and if they were allocated, then also de-allocated. 
/// Thus, it ensures memory safety by guaranteeing that this object must be held 
/// in order to access data stored in these mapped pages, 
/// just like a MutexGuard guarantees that data protected by a Mutex can only be accessed
/// while that Mutex's lock is held. 
#[derive(Debug)]
pub struct MappedPages {
    /// The Frame containing the top-level P4 page table that this MappedPages was originally mapped into. 
    page_table_p4: Frame,
    /// The range of virtual pages contained by this mapping.
    pages: MaybeAllocatedPages,
    // The EntryFlags that define the page permissions of this mapping
    flags: EntryFlags,
}
impl Deref for MappedPages {
    type Target = PageRange;
    fn deref(&self) -> &PageRange {
        self.pages.deref()
    }
}

impl MappedPages {
    /// Returns an empty MappedPages object that performs no allocation or mapping actions. 
    /// Can be used as a placeholder, but will not permit any real usage. 
    pub fn empty() -> MappedPages {
        MappedPages {
            page_table_p4: get_current_p4(),
            pages: MaybeAllocatedPages::NotAllocated(PageRange::empty()),
            flags: Default::default(),
        }
    }

    /// Returns the flags that describe this `MappedPages` page table permissions.
    pub fn flags(&self) -> EntryFlags {
        self.flags
    }

    /// Constructs a MappedPages object from an already existing mapping.
    /// Useful for creating idle task Stacks, for example. 
    // TODO FIXME: remove this function, it's dangerous!!
    #[deprecated]
    pub fn from_existing(already_mapped_pages: PageRange, flags: EntryFlags) -> MappedPages {
        MappedPages {
            page_table_p4: get_current_p4(),
            pages: MaybeAllocatedPages::NotAllocated(already_mapped_pages),
            flags: flags,
        }
    }


    /// Merges the given `MappedPages` object into this `MappedPages` object.
    ///
    /// For example, if you have the following `MappedPages` objects:    
    /// * this mapping, with a page range including one page at 0x2000
    /// * `mp`, with a page range including two pages at 0x3000 and 0x4000
    /// Then this `MappedPages` object will be updated to cover three pages from `[0x2000:0x4000]` inclusive.
    /// 
    /// In addition, the `MappedPages` objects must have the same flags and page table root frame
    /// (i.e., they must have all been mapped using the same set of page tables).
    /// 
    /// In addition, the `MappedPages` objects must either all have AllocatedPages or all have no AllocatedPages.
    /// `MappedPages` that were mapped to allocated virtual pages cannot be merged with those that weren't mapped to allocated pages.
    /// 
    /// If an error occurs, such as the `mappings` not being contiguous or having different flags, 
    /// then a tuple including an error message and the original `mp` will be returned,
    /// which prevents the `mp` from being dropped. 
    /// 
    /// # Note
    /// No remapping actions or page reallocations will occur on either a failure or a success.
    pub fn merge(&mut self, mp: MappedPages) -> Result<(), (&'static str, MappedPages)> {

        let mut previous_end: Page = *self.pages.end(); // start at the end of this mapping

        // first, we need to double check that everything is contiguous and the flags and p4 Frame are the same.
        let mut err: Option<&'static str> = None;

        if mp.page_table_p4 != self.page_table_p4 {
            error!("MappedPages::merge(): mappings weren't mapped using the same page table: {:?} vs. {:?}",
                mp.page_table_p4, self.page_table_p4);
            err = Some("mappings were mapped with different page tables");
        }
        else if mp.flags != self.flags {
            error!("MappedPages::merge(): mappings had different flags: {:?} vs. {:?}",
                mp.flags, self.flags);
            err = Some("mappings were mapped with different flags");
        }
        else if *mp.pages.start() != previous_end + 1 {
            error!("MappedPages::merge(): mappings weren't contiguous in virtual memory: one ends at {:?} and the next starts at {:?}",
                previous_end, mp.pages.start());
            err = Some("mappings were not contiguous in virtual memory");
        } 
        else if mp.pages.is_allocated() != self.pages.is_allocated() {
            error!("MappedPages::merge(): some mapping were mapped to AllocatedPages, while others were not.");
            err = Some("some mappings were mapped to AllocatedPages, while others were not");
        }
        previous_end = *mp.pages.end();
        
        if let Some(e) = err {
            return Err((e,mp));
        }

        // Here, all of our conditions were met, so we can merge the MappedPages object into this one so 
        // that it goes from the first start page to the last end page.

        // to ensure the existing mapping doesn't run its drop handler and unmap those pages
        mem::forget(mp); 
        
        let new_page_range = PageRange::new(*self.pages.start(), previous_end);
        let new_pages = if self.pages.is_allocated(){
            MaybeAllocatedPages::Allocated(AllocatedPages{
                pages: new_page_range,
            })
        } else {
            MaybeAllocatedPages::NotAllocated(new_page_range)
        };
        
        self.pages = new_pages;
        Ok(())
    }


    /// Creates a deep copy of this `MappedPages` memory region,
    /// by duplicating not only the virtual memory mapping
    /// but also the underlying physical memory frames. 
    /// 
    /// The caller can optionally specify new flags for the duplicated mapping,
    /// otherwise, the same flags as the existing `MappedPages` will be used. 
    /// This is useful for when you want to modify contents in the new pages,
    /// since it avoids extra `remap()` operations.
    /// 
    /// Returns a new `MappedPages` object with the same in-memory contents
    /// as this object, but at a completely new memory region.
    pub fn deep_copy<A: FrameAllocator>(&self, new_flags: Option<EntryFlags>, active_table_mapper: &mut Mapper, allocator: &mut A) -> Result<MappedPages, &'static str> {
        let size_in_pages = self.size_in_pages();

        use paging::allocate_pages;
        let new_pages = allocate_pages(size_in_pages).ok_or_else(|| "Couldn't allocate_pages()")?;

        // we must temporarily map the new pages as Writable, since we're about to copy data into them
        let new_flags = new_flags.unwrap_or(self.flags);
        let needs_remapping = new_flags.is_writable(); 
        let mut new_mapped_pages = active_table_mapper.map_allocated_pages(
            new_pages, 
            new_flags | EntryFlags::WRITABLE, // force writable
            allocator
        )?;

        // perform the actual copy of in-memory content
        // TODO: there is probably a better way to do this, e.g., `rep stosq/movsq` or something
        {
            type PageContent = [u8; PAGE_SIZE];
            let source: &[PageContent] = self.as_slice(0, size_in_pages)?;
            let dest: &mut [PageContent] = new_mapped_pages.as_slice_mut(0, size_in_pages)?;
            dest.copy_from_slice(source);
        }

        if needs_remapping {
            new_mapped_pages.remap(active_table_mapper, new_flags)?;
        }
        
        Ok(new_mapped_pages)
    }

    
    /// Change the permissions (`new_flags`) of this `MappedPages`'s page table entries.
    pub fn remap(&mut self, active_table_mapper: &mut Mapper, new_flags: EntryFlags) -> Result<(), &'static str> {
        if self.size_in_pages() == 0 { return Ok(()); }

        if new_flags == self.flags {
            trace!("remap(): new_flags were the same as existing flags, doing nothing.");
            return Ok(());
        }

        for page in self.pages.clone() {
            let p1 = active_table_mapper.p4_mut()
                .next_table_mut(page.p4_index())
                .and_then(|p3| p3.next_table_mut(page.p3_index()))
                .and_then(|p2| p2.next_table_mut(page.p2_index()))
                .ok_or("mapping code does not support huge pages")?;
            
            let frame = p1[page.p1_index()].pointed_frame().ok_or("remap(): page not mapped")?;
            p1[page.p1_index()].set(frame, new_flags | EntryFlags::PRESENT);

            tlb_flush_virt_addr(page.start_address());
        }
        
        if let Some(func) = BROADCAST_TLB_SHOOTDOWN_FUNC.try() {
            func(self.pages.deref().clone());
        }

        self.flags = new_flags;
        Ok(())
    }   


    /// Remove the virtual memory mapping for the given `Page`s.
    /// This should NOT be public because it should only be invoked when a `MappedPages` object is dropped.
    fn unmap<A>(&mut self, active_table_mapper: &mut Mapper, _allocator_ref: &MutexIrqSafe<A>) -> Result<(), &'static str> 
        where A: FrameAllocator
    {
        if self.size_in_pages() == 0 { return Ok(()); }

        for page in self.pages.clone() {            
            let p1 = active_table_mapper.p4_mut()
                .next_table_mut(page.p4_index())
                .and_then(|p3| p3.next_table_mut(page.p3_index()))
                .and_then(|p2| p2.next_table_mut(page.p2_index()))
                .ok_or("mapping code does not support huge pages")?;
            
            let _frame = p1[page.p1_index()].pointed_frame().ok_or("unmap(): page not mapped")?;
            p1[page.p1_index()].set_unused();

            tlb_flush_virt_addr(page.start_address());
            
            // TODO free p(1,2,3) table if empty
            // _allocator_ref.lock().deallocate_frame(frame);
        }
    
        #[cfg(not(bm_map))]
        {
            if let Some(func) = BROADCAST_TLB_SHOOTDOWN_FUNC.try() {
                func(self.pages.deref().clone());
            }
        }

        Ok(())
    }


    /// Reinterprets this `MappedPages`'s underlying memory region as a struct of the given type `T`,
    /// i.e., overlays a struct on top of this mapped memory region. 
    /// 
    /// # Requirements
    /// The type `T` must implement the `FromBytes` trait, which is similar to the requirements 
    /// of a "plain old data" type, in that it cannot contain Rust references (`&` or `&mut`).
    /// This makes sense because there is no valid way to reinterpret a region of untyped memory 
    /// as a Rust reference. 
    /// In addition, if we did permit that, a Rust reference created from unchecked memory contents
    /// could never be valid, safe, or sound, as it could allow random memory access 
    /// (just like with an arbitrary pointer dereference) that could break isolation.
    /// 
    /// To satisfy this condition, you can use `#[derive(FromBytes)]` on your struct type `T`,
    /// which will only compile correctly if the struct can be validly constructed 
    /// from "untyped" memory, i.e., an array of bytes.
    /// 
    /// # Arguments
    /// `offset`: the offset into the memory region at which the struct is located (where it should start).
    /// 
    /// Returns a reference to the new struct (`&T`) that is formed from the underlying memory region,
    /// with a lifetime dependent upon the lifetime of this `MappedPages` object.
    /// This ensures safety by guaranteeing that the returned struct reference 
    /// cannot be used after this `MappedPages` object is dropped and unmapped.
    pub fn as_type<T: FromBytes>(&self, offset: usize) -> Result<&T, &'static str> {
        let size = mem::size_of::<T>();
        if false {
            debug!("MappedPages::as_type(): requested type {} with size {} at offset {}, MappedPages size {}!",
                core::any::type_name::<T>(),
                size, offset, self.size_in_bytes()
            );
        }

        // check that size of the type T fits within the size of the mapping
        let end = offset + size;
        if end > self.size_in_bytes() {
            error!("MappedPages::as_type(): requested type {} with size {} at offset {}, which is too large for MappedPages of size {}!",
                core::any::type_name::<T>(),
                size, offset, self.size_in_bytes()
            );
            return Err("requested type and offset would not fit within the MappedPages bounds");
        }

        // SAFE: we guarantee the size and lifetime are within that of this MappedPages object
        let t: &T = unsafe { 
            &*((self.pages.start_address().value() + offset) as *const T)
        };

        Ok(t)
    }


    /// Same as [`as_type()`](#method.as_type), but returns a *mutable* reference to the type `T`.
    /// 
    /// Thus, it checks to make sure that the underlying mapping is writable.
    pub fn as_type_mut<T: FromBytes>(&mut self, offset: usize) -> Result<&mut T, &'static str> {
        let size = mem::size_of::<T>();
        if false {
            debug!("MappedPages::as_type_mut(): requested type {} with size {} at offset {}, MappedPages size {}!",
                core::any::type_name::<T>(),
                size, offset, self.size_in_bytes()
            );
        }

        // check flags to make sure mutability is allowed (otherwise a page fault would occur on a write)
        if !self.flags.is_writable() {
            error!("MappedPages::as_type_mut(): requested type {} with size {} at offset {}, but MappedPages weren't writable (flags: {:?})",
                core::any::type_name::<T>(),
                size, offset, self.flags
            );
            return Err("as_type_mut(): MappedPages were not writable");
        }
        
        // check that size of type T fits within the size of the mapping
        let end = offset + size;
        if end > self.size_in_bytes() {
            error!("MappedPages::as_type_mut(): requested type {} with size {} at offset {}, which is too large for MappedPages of size {}!",
                core::any::type_name::<T>(),
                size, offset, self.size_in_bytes()
            );
            return Err("requested type and offset would not fit within the MappedPages bounds");
        }

        // SAFE: we guarantee the size and lifetime are within that of this MappedPages object
        let t: &mut T = unsafe {
            &mut *((self.pages.start_address().value() + offset) as *mut T)
        };

        Ok(t)
    }


    /// Reinterprets this `MappedPages`'s underlying memory region as a slice of any type.
    /// 
    /// It has similar type requirements as the [`as_type()`](#method.as_type) method.
    /// 
    /// # Arguments
    /// * `byte_offset`: the offset (in number of bytes) into the memory region at which the slice should start.
    /// * `length`: the length of the slice, i.e., the number of `T` elements in the slice. 
    ///   Thus, the slice will go from `offset` to `offset` + (sizeof(`T`) * `length`).
    /// 
    /// Returns a reference to the new slice that is formed from the underlying memory region,
    /// with a lifetime dependent upon the lifetime of this `MappedPages` object.
    /// This ensures safety by guaranteeing that the returned slice 
    /// cannot be used after this `MappedPages` object is dropped and unmapped.
    pub fn as_slice<T: FromBytes>(&self, byte_offset: usize, length: usize) -> Result<&[T], &'static str> {
        let size_in_bytes = mem::size_of::<T>() * length;
        if false {
            debug!("MappedPages::as_slice(): requested slice of type {} with length {} (total size {}) at byte_offset {}, MappedPages size {}!",
                core::any::type_name::<T>(),
                length, size_in_bytes, byte_offset, self.size_in_bytes()
            );
        }
        
        // check that size of slice fits within the size of the mapping
        let end = byte_offset + (length * mem::size_of::<T>());
        if end > self.size_in_bytes() {
            error!("MappedPages::as_slice(): requested slice of type {} with length {} (total size {}) at byte_offset {}, which is too large for MappedPages of size {}!",
                core::any::type_name::<T>(),
                length, size_in_bytes, byte_offset, self.size_in_bytes()
            );
            return Err("requested slice length and offset would not fit within the MappedPages bounds");
        }

        // SAFE: we guarantee the size and lifetime are within that of this MappedPages object
        let slc: &[T] = unsafe {
            slice::from_raw_parts((self.pages.start_address().value() + byte_offset) as *const T, length)
        };

        Ok(slc)
    }


    /// Same as [`as_slice()`](#method.as_slice), but returns a *mutable* slice. 
    /// 
    /// Thus, it checks to make sure that the underlying mapping is writable.
    pub fn as_slice_mut<T: FromBytes>(&mut self, byte_offset: usize, length: usize) -> Result<&mut [T], &'static str> {
        let size_in_bytes = mem::size_of::<T>() * length;
        if false {
            debug!("MappedPages::as_slice_mut(): requested slice of type {} with length {} (total size {}) at byte_offset {}, MappedPages size {}!",
                core::any::type_name::<T>(), 
                length, size_in_bytes, byte_offset, self.size_in_bytes()
            );
        }
        
        // check flags to make sure mutability is allowed (otherwise a page fault would occur on a write)
        if !self.flags.is_writable() {
            error!("MappedPages::as_slice_mut(): requested mutable slice of type {} with length {} (total size {}) at byte_offset {}, but MappedPages weren't writable (flags: {:?})",
                core::any::type_name::<T>(),
                length, size_in_bytes, byte_offset, self.flags
            );
            return Err("as_slice_mut(): MappedPages were not writable");
        }

        // check that size of slice fits within the size of the mapping
        let end = byte_offset + (length * mem::size_of::<T>());
        if end > self.size_in_bytes() {
            error!("MappedPages::as_slice_mut(): requested mutable slice of type {} with length {} (total size {}) at byte_offset {}, which is too large for MappedPages of size {}!",
                core::any::type_name::<T>(),
                length, size_in_bytes, byte_offset, self.size_in_bytes()
            );
            return Err("requested slice length and offset would not fit within the MappedPages bounds");
        }

        // SAFE: we guarantee the size and lifetime are within that of this MappedPages object
        let slc: &mut [T] = unsafe {
            slice::from_raw_parts_mut((self.pages.start_address().value() + byte_offset) as *mut T, length)
        };

        Ok(slc)
    }


    /// Reinterprets this `MappedPages`'s underlying memory region as an executable function with any signature.
    /// 
    /// # Arguments
    /// * `offset`: the offset (in number of bytes) into the memory region at which the function starts.
    /// * `space`: a hack to satisfy the borrow checker's lifetime requirements.
    /// 
    /// Returns a reference to the function that is formed from the underlying memory region,
    /// with a lifetime dependent upon the lifetime of the given `space` object. 
    ///
    /// TODO FIXME: this isn't really safe as it stands now. 
    /// Ideally, we need to have an integrated function that checks with the mod_mgmt crate 
    /// to see if the size of the function can fit (not just the size of the function POINTER, which will basically always fit)
    /// within the bounds of this `MappedPages` object;
    /// this integrated function would be based on the given string name of the function, like "task::this::foo",
    /// and would invoke this as_func() function directly.
    /// 
    /// We have to accept space for the function pointer to exist, because it cannot live in this function's stack. 
    /// It has to live in stack of the function that invokes the actual returned function reference,
    /// otherwise there would be a lifetime issue and a guaranteed page fault. 
    /// So, the `space` arg is a hack to ensure lifetimes;
    /// we don't care about the actual value of `space`, as the value will be overwritten,
    /// and it doesn't matter both before and after the call to this `as_func()`.
    /// 
    /// The generic `F` parameter is the function type signature itself, e.g., `fn(String) -> u8`.
    /// 
    /// # Examples
    /// Here's how you might call this function:
    /// ```
    /// type PrintFuncSignature = fn(&str) -> Result<(), &'static str>;
    /// let mut space = 0; // this must persist throughout the print_func being called
    /// let print_func: &PrintFuncSignature = mapped_pages.as_func(func_offset, &mut space).unwrap();
    /// print_func("hi");
    /// ```
    /// Because Rust has lexical lifetimes, the `space` variable must have a lifetime at least as long as the  `print_func` variable,
    /// meaning that `space` must still be in scope in order for `print_func` to be invoked.
    /// 
    #[doc(hidden)]
    pub fn as_func<'a, F>(&self, offset: usize, space: &'a mut usize) -> Result<&'a F, &'static str> {
        let size = mem::size_of::<F>();
        if true {
            #[cfg(not(downtime_eval))]
            debug!("MappedPages::as_func(): requested {} with size {} at offset {}, MappedPages size {}!",
                core::any::type_name::<F>(),
                size, offset, self.size_in_bytes()
            );
        }

        // check flags to make sure these pages are executable (otherwise a page fault would occur when this func is called)
        if !self.flags.is_executable() {
            error!("MappedPages::as_func(): requested {}, but MappedPages weren't executable (flags: {:?})",
                core::any::type_name::<F>(),
                self.flags
            );
            return Err("as_func(): MappedPages were not executable");
        }

        // check that size of the type F fits within the size of the mapping
        let end = offset + size;
        if end > self.size_in_bytes() {
            error!("MappedPages::as_func(): requested type {} with size {} at offset {}, which is too large for MappedPages of size {}!",
                core::any::type_name::<F>(),
                size, offset, self.size_in_bytes()
            );
            return Err("requested type and offset would not fit within the MappedPages bounds");
        }

        *space = self.pages.start_address().value() + offset; 

        // SAFE: we guarantee the size and lifetime are within that of this MappedPages object
        let t: &'a F = unsafe {
            mem::transmute(space)
        };

        Ok(t)
    }
}


/// A convenience function that exposes the `MappedPages::unmap` function
/// (which is normally hidden/non-public because it's typically called from the Drop handler)
/// for usage from testing/benchmark code for the memory mapping evaluation.
#[cfg(mapper_spillful)]
pub fn mapped_pages_unmap<A: FrameAllocator>(
    mapped_pages: &mut MappedPages,
    mapper: &mut Mapper,
    allocator_ref: &super::FrameAllocatorRef<A>, 
) -> Result<(), &'static str> {
    mapped_pages.unmap(mapper, allocator_ref)
}


impl Drop for MappedPages {
    fn drop(&mut self) {
        if self.size_in_pages() == 0 { return; }
        // trace!("MappedPages::drop(): unmapping MappedPages start: {:?} to end: {:?}", self.pages.start(), self.pages.end());

        let mut mapper = Mapper::from_current();
        if mapper.target_p4 != self.page_table_p4 {
            error!("BUG: MappedPages::drop(): {:?}\n    current P4 {:?} must equal original P4 {:?}, \
                cannot unmap MappedPages from a different page table than they were originally mapped to!",
                self, get_current_p4(), self.page_table_p4
            );
            return;
        }   

        let frame_allocator_ref = match get_frame_allocator_ref() {
            Some(fa) => fa,
            _ => {
                error!("MappedPages::drop(): couldn't get frame allocator!");
                return;
            }
        };
        
        if let Err(e) = self.unmap(&mut mapper, &frame_allocator_ref) {
            error!("MappedPages::drop(): failed to unmap, error: {:?}", e);
        }

        // Note that the AllocatedPages will automatically be dropped here too,
        // we do not need to call anything to make that happen
    }
}

