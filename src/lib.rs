use patient_waiter::{CountingWaiter, PatientWaiter, ValidateResult};
use std::cell::{UnsafeCell, Cell};
use std::fmt::{Debug, Formatter};
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

// Free for the taking!
const FLAGS_FREE: u8 = 0b00;
// Taken, should wait on condvar to wake up.
const FLAGS_TAKEN: u8 = 0b01;
// Spinlock would be most efficient here.
const FLAGS_TEMP: u8 = 0b10;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum LockState {
    Free,
    Taken,
    Temp,
}

impl From<u8> for LockState {
    fn from(x: u8) -> Self {
        match x & 0b11 {
            FLAGS_TAKEN => LockState::Taken,
            FLAGS_TEMP => LockState::Temp,
            FLAGS_FREE => LockState::Free,
            0b11 => panic!(),
            _ => unreachable!(),
        }
    }
}

impl LockState {
    pub fn to_u8(self) -> u8 {
        match self {
            LockState::Free => FLAGS_FREE,
            LockState::Temp => FLAGS_TEMP,
            LockState::Taken => FLAGS_TAKEN,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
struct LinkLockState {
    prev: LockState,
    post: LockState,
    link_taken: bool,
}

impl LinkLockState {
    pub(crate) fn from_u8(x: u8) -> Self {
        let prev = x.into();
        let post = (x >> 2).into();
        let taken = (x >> 4) > 0;

        Self {
            prev,
            post,
            link_taken: taken,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        (self.link_taken as u8) << 4 | self.post.to_u8() << 2 | self.prev.to_u8()
    }
}

enum LockReturn {
    Success { post: usize, prev: usize },
    WaitOnMutex,
    BackOff,
}

/// Center must not be taken.
///
/// It is expected that if this returns back off, a user will continuously
/// poll it until it does not return back off.
///
/// The intention is that if all values were to continuously try to acquire,
/// then the spin locks would deadlock. Making every other index forcibly
/// back off in case of contention would eliminate the issue.
// Backs off of prev. Remains persistent on post.
fn temp_lock_wait_three<T: Debug>(center: &Link<T>, others: &[ManuallyDrop<Link<T>>]) -> LockReturn {
    if let None = temp_lock_wait_one(center) {
        return LockReturn::WaitOnMutex;
    }


    temp_lock_left_right(center, others)
}

fn temp_lock_left_right<T: Debug>(center: &Link<T>, others: &[ManuallyDrop<Link<T>>]) -> LockReturn {
    // Now that we've acquired the lock, these should remain
    // constant for the remainder of the function.
    let prev = center.prev.load(Ordering::Relaxed);
    let post = center.post.load(Ordering::Relaxed);
    if prev == post && prev == center.index {
        return LockReturn::Success { prev, post };
    }

    // Deal with prev first.
    let mut wanted_prev = LinkLockState {
        prev: LockState::Free,
        post: LockState::Temp,
        link_taken: false,
    };

    let mut expected_prev = LinkLockState {
        prev: LockState::Free,
        post: LockState::Free,
        link_taken: false,
    };

    while let Err(other_state) = others[prev].lock_state.compare_exchange(
        expected_prev.to_u8(),
        wanted_prev.to_u8(),
        Ordering::Acquire,
        Ordering::Relaxed,
    ) {
        let state = LinkLockState::from_u8(other_state);

        if state.link_taken || matches!(state.post, LockState::Taken) {
            unlock_one(center);
            return LockReturn::WaitOnMutex;
        }

        if matches!(state.post, LockState::Temp) {
            unlock_one(center);
            return LockReturn::BackOff;
        }

        wanted_prev.prev = state.prev;
        expected_prev.prev = state.prev;
    }

    if prev == post {
        return LockReturn::Success { prev, post }
    }

    // Deal with post next.
    let wanted_post = LinkLockState {
        prev: LockState::Temp,
        post: LockState::Free,
        link_taken: false,
    };

    let expected_post = LinkLockState {
        prev: LockState::Free,
        post: LockState::Free,
        link_taken: false,
    };

    while let Err(other_state) = others[post].lock_state.compare_exchange(
        expected_post.to_u8(),
        wanted_post.to_u8(),
        Ordering::Acquire,
        Ordering::Relaxed,
    ) {
        let state = LinkLockState::from_u8(other_state);

        if matches!(state.prev, LockState::Taken) {
            unlock_one(center);
            others[prev]
                .lock_state
                .fetch_update(Ordering::Release, Ordering::Relaxed, |x| {
                    let mut state = LinkLockState::from_u8(x);

                    state.post = LockState::Free;
                    Some(state.to_u8())
                })
                .unwrap();
            return LockReturn::WaitOnMutex;
        }

        wanted_prev.post = state.post;
        expected_prev.post = state.post;

        // std::hint::spin_loop();
    }

    LockReturn::Success { prev, post }
}

/// Locks this and next.
///
/// It is expected that if this returns back off, a user will continuously
/// poll it until it does not return back off.
///
/// See [`temp_lock_wait_three`] for more details on back off.
// Once again, left back off, right press forward.
fn temp_lock_wait_two<T: Debug>(left: &Link<T>, others: &[ManuallyDrop<Link<T>>]) -> LockReturn {
    // Deal with prev first.
    let mut wanted_prev = LinkLockState {
        prev: LockState::Free,
        post: LockState::Temp,
        link_taken: false,
    };

    let mut expected_prev = LinkLockState {
        prev: LockState::Free,
        post: LockState::Free,
        link_taken: false,
    };

    while let Err(other_state) = left.lock_state.compare_exchange(
        expected_prev.to_u8(),
        wanted_prev.to_u8(),
        Ordering::Acquire,
        Ordering::Relaxed,
    ) {
        let state = LinkLockState::from_u8(other_state);

        if state.link_taken || matches!(state.post, LockState::Taken) {
            return LockReturn::WaitOnMutex;
        }

        if matches!(state.post, LockState::Temp) {
            return LockReturn::BackOff;
        }

        wanted_prev.prev = state.prev;
        expected_prev.prev = state.prev;
    }

    let post = left.post.load(Ordering::Relaxed);
    if post == left.index {
        return LockReturn::Success {
            prev: left.index,
            post,
        };
    }

    // Deal with post next.
    let wanted_post = LinkLockState {
        prev: LockState::Temp,
        post: LockState::Free,
        link_taken: false,
    };

    let expected_post = LinkLockState {
        prev: LockState::Free,
        post: LockState::Free,
        link_taken: false,
    };

    while let Err(other_state) = others[post].lock_state.compare_exchange(
        expected_post.to_u8(),
        wanted_post.to_u8(),
        Ordering::Acquire,
        Ordering::Relaxed,
    ) {
        let state = LinkLockState::from_u8(other_state);

        if matches!(state.prev, LockState::Taken) {
            left.lock_state
                .fetch_update(Ordering::Release, Ordering::Relaxed, |x| {
                    let mut state = LinkLockState::from_u8(x);

                    state.post = LockState::Free;
                    Some(state.to_u8())
                })
                .unwrap();
            return LockReturn::WaitOnMutex;
        }

        wanted_prev.post = state.post;
        expected_prev.post = state.post;

        // std::hint::spin_loop();
    }

    LockReturn::Success {
        prev: left.index,
        post,
    }
}

/// Locks this one only. Useful for temporary inspection.
// Never back off.
fn temp_lock_wait_one<T>(center: &Link<T>) -> Option<()> {
    let wanted_center = LinkLockState {
        prev: LockState::Temp,
        post: LockState::Temp,
        link_taken: true,
    }
    .to_u8();

    let expected_center = LinkLockState {
        prev: LockState::Free,
        post: LockState::Free,
        link_taken: false,
    }
    .to_u8();

    // Do not back off of center.
    while let Err(other_state) = center.lock_state.compare_exchange(
        expected_center,
        wanted_center,
        Ordering::Acquire,
        Ordering::Relaxed,
    ) {
        let state = LinkLockState::from_u8(other_state);

        if state.link_taken
            || matches!(state.prev, LockState::Taken)
            || matches!(state.post, LockState::Taken)
        {
            return None;
        }

        // std::hint::spin_loop();
    }

    Some(())
}

fn lock_three<T: Debug>(center: &Link<T>, others: &[ManuallyDrop<Link<T>>]) -> Option<(usize, usize)> {
    loop {

        match temp_lock_wait_three(center, others) {
            LockReturn::WaitOnMutex => return None,
            LockReturn::BackOff => continue,
            LockReturn::Success { prev, post } => return Some((prev, post)),
        }
    }
}

fn lock_two<T: Debug>(left: &Link<T>, others: &[ManuallyDrop<Link<T>>]) -> Option<usize> {
    loop {
        match temp_lock_wait_two(left, others) {
            LockReturn::WaitOnMutex => return None,
            LockReturn::BackOff => {
                continue;
            },
            LockReturn::Success { post, .. } => return Some(post),
        }
    }
}

fn unlock_one<T: Debug>(link: &Link<T>) {
    link.lock_state.store(
        LinkLockState {
            prev: LockState::Free,
            post: LockState::Free,
            link_taken: false,
        }
        .to_u8(),
        Ordering::Release,
    );
}

fn unlock_prev_post<T>(center: &Link<T>, others: &[ManuallyDrop<Link<T>>]) {
    let prev = center.prev.load(Ordering::Relaxed);
    let post = center.post.load(Ordering::Relaxed);

    if !(prev == post && prev == center.index) {
        others[prev]
            .lock_state
            .fetch_update(Ordering::Release, Ordering::Relaxed, |x| {
                let mut state = LinkLockState::from_u8(x);
                assert!(matches!(state.post, LockState::Taken | LockState::Temp));
                state.post = LockState::Free;
                Some(state.to_u8())
            })
            .unwrap();

        if prev == post {
            return;
        }

        others[post]
            .lock_state
            .fetch_update(Ordering::Release, Ordering::Relaxed, |x| {
                let mut state = LinkLockState::from_u8(x);
                assert!(matches!(state.prev, LockState::Taken | LockState::Temp));
                state.prev = LockState::Free;
                Some(state.to_u8())
            })
            .unwrap();
    }
}

/// The hook parameter runs to allow the caller to ensure they have
/// access to stable prev/post indices.
fn remove<T: Debug>(
    index: usize,
    others: &[ManuallyDrop<Link<T>>],
    hook: impl FnOnce(usize, usize),
) -> Option<Link<T>> {

    let link = &*others[index];

    let (prev, post) = lock_three(link, others)?;

    hook(prev, post);

    let val = remove_unchecked(link, prev, post, others);

    Some(val)
}

/// The hook parameter runs to allow the caller to ensure they have
/// access to stable prev/post indices.
fn remove_prelocked<T: Debug>(
    index: usize,
    others: &[ManuallyDrop<Link<T>>],
    hook: impl FnOnce(usize, usize),
) -> Option<Link<T>> {
    let link = &*others[index];

    // todo: replace with proper mutex.
    let (prev, post) = match temp_lock_left_right(link, others) {
        LockReturn::BackOff => {
            unlock_one(link);
            return remove(index, others, hook);
        }
        LockReturn::Success { prev, post } => (prev, post),
        LockReturn::WaitOnMutex => return None,
    };

    hook(prev, post);

    let val = remove_unchecked(link, prev, post, others);

    Some(val)
}

fn remove_unchecked<T: Debug>(
    center: &Link<T>,
    prev: usize,
    post: usize,
    others: &[ManuallyDrop<Link<T>>],
) -> Link<T> {
    others[prev]
        .post
        .compare_exchange(center.index, post, Ordering::Relaxed, Ordering::Relaxed)
        .unwrap();

    others[post]
        .prev
        .compare_exchange(center.index, prev, Ordering::Relaxed, Ordering::Relaxed)
        .unwrap();

    center
        .lock_state
        .compare_exchange(
            LinkLockState {
                prev: LockState::Temp,
                post: LockState::Temp,
                link_taken: true,
            }
            .to_u8(),
            LinkLockState {
                prev: LockState::Taken,
                post: LockState::Taken,
                link_taken: true,
            }
            .to_u8(),
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .unwrap();

    if !(prev == post && prev == center.index) {
        unlock_prev_post(center, others);
    }

    center.prev.store(center.index, Ordering::Relaxed);
    center.post.store(center.index, Ordering::Relaxed);

    unsafe { std::ptr::read(center) }
}

fn write_into<T: Debug>(
    link: &Link<T>,
    val: Link<T>,
) {
    let Link {
        post,
        prev,
        index,
        data,
        ..
    } = val;

    assert_eq!(index, link.index);

    unsafe {
        std::ptr::write(link.data.get(), data.into_inner());
    }


    link
        .post
        .store(post.into_inner(), Ordering::Relaxed);
    link
        .prev
        .store(prev.into_inner(), Ordering::Relaxed);

    let lock_state = LinkLockState {
        prev: LockState::Free,
        post: LockState::Free,
        link_taken: false,
    }
        .to_u8();

    let old_state = LinkLockState {
        prev: LockState::Taken,
        post: LockState::Taken,
        link_taken: true,
    }
        .to_u8();

    link
        .lock_state
        .compare_exchange(old_state, lock_state, Ordering::Relaxed, Ordering::Relaxed)
        .unwrap();
}

fn insert_after<T: Debug>(
    index: usize,
    others: &[ManuallyDrop<Link<T>>],
    val: Link<T>,
) -> Result<(), Link<T>> {
    let left = &*others[index];

    // todo: replace with mutex when necessary.
    let right = match lock_two(left, others) {
        Some(x) => x,
        None => return Err(val),
    };

    let val_index = val.index;

    val.prev.store(index, Ordering::Relaxed);

    val.post.store(right, Ordering::Relaxed);


    write_into(&others[val_index], val);


    left.post
        .compare_exchange(right, val_index, Ordering::Relaxed, Ordering::Relaxed)
        .unwrap();

    others[right]
        .prev
        .compare_exchange(index, val_index, Ordering::Relaxed, Ordering::Relaxed)
        .unwrap();

    unlock_prev_post(&others[val_index], others);

    Ok(())
}

fn inspect<T: Debug, F, R>(link: &Link<T>, f: F) -> Option<R>
where
    F: FnOnce(&T) -> R,
{
    temp_lock_wait_one(link)?;

    let val = f(unsafe { &*link.data.get() });

    link.lock_state
        .compare_exchange(
            LinkLockState {
                prev: LockState::Temp,
                post: LockState::Temp,
                link_taken: true,
            }
            .to_u8(),
            LinkLockState {
                prev: LockState::Free,
                post: LockState::Free,
                link_taken: false,
            }
            .to_u8(),
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .unwrap();

    Some(val)
}

enum InspectUpgradeResult<T> {
    Success(Link<T>),
    DidNotUpgrade,
    CouldNotAcquireOne,
    BackOffOrMutex,
    LeaveLocked,
}

enum UpgradeMode {
    LeaveLocked,
    Remove,
    False,
}

fn inspect_upgrade<T: Debug, F>(
    link: &Link<T>,
    others: &[ManuallyDrop<Link<T>>],
    f: F,
) -> InspectUpgradeResult<T>
where
    F: FnOnce(&T) -> UpgradeMode,
{
    if let None = temp_lock_wait_one(link) {
        return InspectUpgradeResult::CouldNotAcquireOne;
    }

    match f(unsafe { &*link.data.get() }) {
        UpgradeMode::False => {
            unlock_one(link);
            InspectUpgradeResult::DidNotUpgrade
        },
        UpgradeMode::LeaveLocked => InspectUpgradeResult::LeaveLocked,
        UpgradeMode::Remove => match temp_lock_left_right(link, others) {
            LockReturn::WaitOnMutex | LockReturn::BackOff => InspectUpgradeResult::BackOffOrMutex,
            LockReturn::Success { prev, post } => {
                let val = remove_unchecked(link, prev, post, others);

                InspectUpgradeResult::Success(val)
            }
        },
    }
}

pub struct Link<T> {
    post: AtomicUsize,
    prev: AtomicUsize,
    lock_state: AtomicU8,
    index: usize,
    data: UnsafeCell<T>,
}

impl<T: Debug> Debug for Link<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let state = LinkLockState::from_u8(self.lock_state.load(Ordering::SeqCst));

        let mut f = f.debug_struct(std::any::type_name::<Self>());

        struct Opaque;

        impl Debug for Opaque {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                write!(f, "<locked>")
            }
        }

        f.field("lock_state", &state).field("index", &self.index);

        // inspect(self, |val| {
        //     f.field("value", val);
        // })
        // .unwrap_or_else(|| { f.field("value", &Opaque); });
        f.field("value", &Opaque);

        f
            .field("prev", &self.prev.load(Ordering::Relaxed))
            .field("post", &self.post.load(Ordering::Relaxed));

        f.finish()
    }
}

impl<T> Link<T> {
    fn new(val: T, index: usize, n: usize) -> Self {
        Self {
            post: AtomicUsize::new((index + 1) % n),
            prev: AtomicUsize::new((index as i128 - 1).rem_euclid(n as i128) as usize),
            lock_state: AtomicU8::new(
                LinkLockState {
                    prev: LockState::Free,
                    post: LockState::Free,
                    link_taken: false,
                }
                .to_u8(),
            ),
            index,
            data: UnsafeCell::new(val),
        }
    }

    pub fn original_index(&self) -> usize {
        self.index
    }
}

impl<T> Deref for Link<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.data.get() }
    }
}

impl<T> DerefMut for Link<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.data.get() }
    }
}

pub struct AtomicDeque<T, const N: usize> {
    items: [ManuallyDrop<Link<T>>; N],
    first: AtomicUsize,
    first_locked: AtomicBool,
    len: AtomicUsize,
    patient_waiter: PatientWaiter,
    counter_waiter: CountingWaiter,
}

impl<T: Debug, const N: usize> AtomicDeque<T, N> {
    pub fn new(data: [T; N]) -> Self {
        assert_ne!(N, 0);

        let mut idx = 0;
        Self {
            items: data.map(|x| {
                let this_idx = idx;
                idx += 1;
                ManuallyDrop::new(Link::new(x, this_idx, N))
            }),
            first: AtomicUsize::new(0),
            first_locked: AtomicBool::new(false),
            len: AtomicUsize::new(N),
            patient_waiter: PatientWaiter::new(),
            counter_waiter: CountingWaiter::new(),
        }
    }

    // This should not deadlock or miss a condvar notification, since it will
    // only wait if there are no more items in the list, and a notification would
    // only be fired if someone were to return an item, however that cannot be
    // done without acquiring the first_taken_state lock.
    pub fn next_wait(&self) -> Link<T> {
        while let Err(_) =
            self.first_locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            // std::hint::spin_loop();
        }


        match self
            .len
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| x.checked_sub(1))
        {
            Ok(_) => {}
            Err(_) => unsafe {
                self.patient_waiter.wait_until(
                    || self.first_locked.store(false, Ordering::Relaxed),
                    || self.len.load(Ordering::Relaxed) > 0,
                    || {
                        while let Err(_) = self.first_locked.compare_exchange(
                            false,
                            true,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        ) {
                            // std::hint::spin_loop();
                        }
                        if self
                            .len
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| {
                                x.checked_sub(1)
                            })
                            .is_ok()
                        {
                            ValidateResult::Success
                        } else {
                            ValidateResult::Retry
                        }
                    },
                );
            },
        }

        let first = self.first.load(Ordering::Relaxed);
        let result = remove(first, &self.items, |_, post| {
            self.first.store(post, Ordering::SeqCst);
        });

        self.first_locked.store(false, Ordering::Relaxed);
        result.unwrap()
    }

    pub fn next_try(&self) -> Option<Link<T>> {
        while let Err(_) =
            self.first_locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            // std::hint::spin_loop();
        }

        if let Err(_) = self
            .len
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| x.checked_sub(1))
        {
            self.first_locked.store(false, Ordering::Relaxed);
            return None;
        }

        let first = self.first.load(Ordering::Relaxed);
        let result = remove(first, &self.items, |_, post| {
            self.first.store(post, Ordering::SeqCst)
        });
        self.first_locked.store(false, Ordering::Relaxed);
        result
    }

    fn next_try_prelocked(&self) -> Option<Link<T>> {
        if let Err(_) =
            self.first_locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            return None;
        }

        if let Err(_) = self
            .len
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| x.checked_sub(1))
        {
            panic!("Invalid state!");
        }

        let first = self.first.load(Ordering::Relaxed);
        let result = remove_prelocked(first, &self.items, |_, post| {
            self.first.store(post, Ordering::SeqCst)
        });
        self.first_locked.store(false, Ordering::Relaxed);
        result
    }

    pub fn deposit(&self, link: Link<T>) {
        while let Err(_) =
        self.first_locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {}

        if self.len.load(Ordering::Relaxed) == 0 {
            let index = link.index;

            write_into(&self.items[index], link);

            self.first.store(index, Ordering::Relaxed);

            self
                .first_locked
                .compare_exchange(true, false, Ordering::Release, Ordering::Relaxed)
                .unwrap();
        } else {
            insert_after(self.first.load(Ordering::Relaxed), &self.items, link).expect("Invalid state!");
        }

        self
            .len
            .fetch_add(1, Ordering::Relaxed);

        self.counter_waiter
            .notify();
        self.patient_waiter
            .notify();
    }

    pub fn predicate_next_wait(&self, mut pred: impl FnMut(&T, usize) -> bool) -> Option<Link<T>> {
        let mut checked_count = 0;

        #[derive(Copy, Clone)]
        enum ItemState {
            False,
            Queued,
            Unchecked,
        }

        const INIT_VAL: Cell<ItemState> = Cell::new(ItemState::Unchecked);
        let checked_idx = [INIT_VAL; N];

        let mut token = self.counter_waiter.token();

        loop {
            for ((index, item), state) in self.items.iter().enumerate().zip(&checked_idx) {
                match state.get() {
                    ItemState::False => continue,
                    ItemState::Unchecked => match inspect_upgrade(&*item, &self.items, |item| {
                        if pred(item, index) {
                            if index == self.first.load(Ordering::Relaxed) {
                                UpgradeMode::LeaveLocked
                            } else {
                                UpgradeMode::Remove
                            }
                        } else {
                            UpgradeMode::False
                        }
                    }) {
                        InspectUpgradeResult::Success(x) => return Some(x),
                        InspectUpgradeResult::BackOffOrMutex => state.set(ItemState::Queued),
                        InspectUpgradeResult::CouldNotAcquireOne => {}
                        InspectUpgradeResult::DidNotUpgrade => {
                            checked_count += 1;
                            state.set(ItemState::False)
                        },
                        InspectUpgradeResult::LeaveLocked => match self.next_try_prelocked() {
                            Some(x) => return Some(x),
                            None => state.set(ItemState::Queued),
                        },
                    },
                    ItemState::Queued => {
                        if index == self.first.load(Ordering::Relaxed) {
                            match self.next_try() {
                                Some(link) => {
                                    if link.index == index {
                                        return Some(link);
                                    } else if let ItemState::Queued = checked_idx[index].get() {
                                        return Some(link);
                                    } else {
                                        self.deposit(link);
                                    }
                                }
                                None => {}
                            }
                        } else {
                            match remove(index, &self.items, |_, _| {}) {
                                Some(x) => return Some(x),
                                None => {}
                            }
                        }
                    }
                }
            }

            if checked_count != N {
                self.counter_waiter.wait(&mut token);
            } else {
                break;
            }
        }

        None
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }
}

// We do literally "give out" `T`, so this is the perfect opportunity
// to only require `T: Send`.
unsafe impl<T: Send, const N: usize> Sync for AtomicDeque<T, N> {}
unsafe impl<T: Send, const N: usize> Send for AtomicDeque<T, N> {}

unsafe impl<T: Sync> Sync for Link<T> {}
unsafe impl<T: Send> Send for Link<T> {}

impl<T, const N: usize> Drop for AtomicDeque<T, N> {
    fn drop(&mut self) {
        self.items.iter().for_each(|link| {
            if !LinkLockState::from_u8(link.lock_state.load(Ordering::Relaxed)).link_taken {
                unsafe { std::ptr::drop_in_place(link.data.get()) }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::AtomicDeque;
    use hushed_panic::hush_this_test;
    use std::sync::Arc;

    #[test]
    fn it_works() {
        let buffer = AtomicDeque::new([10usize]);

        assert_eq!(buffer.len(), 1);

        let next = buffer.next_try().unwrap();

        assert_eq!(*next, 10);

        assert_eq!(buffer.len(), 0);
        buffer.deposit(next);
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn filtered_take() {
        let buffer = AtomicDeque::new([0, 1, 2, 3, 4]);

        let a = buffer.predicate_next_wait(|x, _| x % 2 == 1).unwrap();
        let b = buffer.predicate_next_wait(|x, _| x % 2 == 1).unwrap();

        assert_eq!(*a, 1);
        assert_eq!(*b, 3);

        buffer.deposit(a);

        buffer.deposit(b);

        let c = buffer.predicate_next_wait(|x, _| x % 2 == 1).unwrap();
        assert_eq!(*c, 1);
    }

    #[test]
    #[should_panic]
    fn zero_items() {
        let _x = hush_this_test();
        AtomicDeque::new([(); 0]);
    }

    #[test]
    #[should_panic]
    fn deposit_overflow() {
        let _x = hush_this_test();
        let buffer_1 = AtomicDeque::new([10usize, 30]);
        let buffer_2 = AtomicDeque::new([10usize]);

        let link = buffer_2.next_wait();
        buffer_1.deposit(link);
    }

    #[test]
    #[should_panic]
    fn not_taken() {
        let _x = hush_this_test();
        let buffer_1 = AtomicDeque::new([10usize, 30]);
        let buffer_2 = AtomicDeque::new([10usize, 20]);

        let _link1 = buffer_2.next_wait();
        let link2 = buffer_2.next_wait();
        let _link3 = buffer_1.next_wait();

        buffer_1.deposit(link2);
    }

    #[test]
    fn stress_test() {
        let buffer = Arc::new(AtomicDeque::new([(); 3]));
        let handles = (0..6)
            .map(|_| {
                let buffer = buffer.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        buffer.deposit(buffer.next_wait());
                    }
                    println!("Done!");
                })
            })
            .collect::<Vec<_>>();

        handles
            .into_iter()
            .for_each(|x| x.join().unwrap());
    }
}
