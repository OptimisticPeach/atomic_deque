use std::sync::Arc;
use parking_lot::Mutex;
use atomic_deque::AtomicDeque;

fn main() {
    // let start = std::time::Instant::now();
    let buffer = Arc::new(AtomicDeque::new([String::from("abc"), String::from("def"), String::from("ghi")]));
    let mut handles = (0..7)
        .map(|_| {
            let buffer = buffer.clone();
            std::thread::spawn(move || {
                for _ in 0..500000 {
                    buffer.deposit(buffer.next_wait());
                }
            })
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .for_each(|x| x.join().unwrap());
    // let end1 = start.elapsed();
    //
    // let start = std::time::Instant::now();
    // let buffer = Arc::new(Mutex::new(vec![String::from("abc"), String::from("def"), String::from("ghi")]));
    // let mut handles = (0..50)
    //     .map(|_| {
    //         let buffer = buffer.clone();
    //         std::thread::spawn(move || {
    //             let mut buffer = buffer.lock();
    //             for _ in 0..5000 {
    //                 let val = buffer.pop().unwrap();
    //                 buffer.push(val);
    //             }
    //         })
    //     })
    //     .collect::<Vec<_>>();
    //
    // handles
    //     .into_iter()
    //     .for_each(|x| x.join().unwrap());
    // let end2 = start.elapsed();
    //
    // let start = std::time::Instant::now();
    // let buffer = Arc::new(Mutex::new(vec![String::from("abc"), String::from("def"), String::from("ghi")]));
    // let mut handles = (0..50)
    //     .map(|_| {
    //         let buffer = buffer.clone();
    //         std::thread::spawn(move || {
    //             for _ in 0..5000 {
    //                 let mut buffer = buffer.lock();
    //                 let val = buffer.pop().unwrap();
    //                 buffer.push(val);
    //             }
    //         })
    //     })
    //     .collect::<Vec<_>>();
    //
    // handles
    //     .into_iter()
    //     .for_each(|x| x.join().unwrap());
    // let end3 = start.elapsed();
    //
    // println!("my_library: {:?}, mutex_around_loop: {:?}, mutex_in_loop: {:?}", end1, end2, end3);
}
