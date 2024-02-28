use axum::body::BodyDataStream;
use axum::http::{HeaderValue, StatusCode};
use futures::TryStreamExt;
use futures_core::stream::Stream;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher, DefaultHasher};
use std::io;
use std::sync::Mutex;
use std::task::Waker;
use tokio::io::AsyncReadExt;
use tokio_util::io::StreamReader;

const BUFFERED_LEN: usize = 32 * 1024; // max length that is written/read to/from intermediate buffers
const CHUNK_SIZE: usize = 8 * 1024 * 1024; // size of 1 chunk of file in memory
const MAX_CHUNK_NUM: usize = 512; // maximum number of chunks per 1 file
const MAX_FILE_SIZE: usize = CHUNK_SIZE * MAX_CHUNK_NUM;
const DB_LENGTH: usize = 1000003; // length of hash table containing files, should be a prime number
const MAX_REHASH_NUM: usize = 5; // how many times to rehash if place in hash table occupied (number of potential spots for 1 file path)

// represents a file in the hash table
pub struct DbItem {
    chunks: Vec<Vec<u8>>, // stores data chunks
    has_writer: bool, // indicates whether file is currently being written to
    mtx: Mutex<u8>,
    name: String, // path to the file
    len: usize, // length of currently available data
    wake_me: bool, // signals to writer there is a waiter in variable waiter
    waking: bool, // signals to writer that waiters are currently being awoken
    waiter: Option<Waker>, // the chosen waiting reader, wakes others
    waiters: VecDeque<Waker>, // queue of waiting readers
    for_deletion: bool, // marks item for deletion
    content_type: HeaderValue, // stores file content type
}

impl DbItem {
    fn new() -> DbItem {
        DbItem {
            chunks: Vec::new(),
            has_writer: false,
            mtx: Mutex::new(0),
            name: String::new(),
            len: 0,
            wake_me: false,
            waking: false,
            waiter: None,
            waiters: VecDeque::new(),
            for_deletion: false,
            content_type: HeaderValue::from_str("").unwrap(),
        }
    }
}

// only used for simplicity initializing the DB below
// obviously wrong to do this, though correct cloning doesn't make much
impl Clone for DbItem {
    fn clone(&self) -> Self {
        DbItem::new()
    }
}

// a disgusting global variable for some unsafe operations
static mut DB: Vec<DbItem> = Vec::new(); // stores files and information about them as a simple hash table

// called at the start of main to init the hash table of files
pub fn init_db() {
    unsafe {
        DB = vec![DbItem::new(); DB_LENGTH];
    }
}

// finds file id in hash table for GET requests
pub async fn get_item<'a>(name: String) -> (&'a mut DbItem, usize, bool, HeaderValue) {
    let hashed_names = get_name_hashes(&name);
    unsafe {
        for mut hashed_name in hashed_names {
            hashed_name %= DB_LENGTH;
            if DB[hashed_name].name == name {
                if DB[hashed_name].for_deletion {
                    return (&mut DB[hashed_name], usize::MAX, false, HeaderValue::from_str("").unwrap());
                } else {
                    return (
                        &mut DB[hashed_name],
                        hashed_name,
                        DB[hashed_name].has_writer,
                        DB[hashed_name].content_type.clone(),
                    );
                }
            }
        }
        (&mut DB[0], usize::MAX, false, HeaderValue::from_str("").unwrap())
    }
}

// marks item for deletion
fn mark_for_deletion(item: &mut DbItem) {
    let _lock = item.mtx.lock();
    item.for_deletion = true;
    if !item.has_writer {
        // combination of no writer and 0 len stops all readers
        item.len = 0; // we would let the writer do this since he accesses it without mutex
    }
}

// frees space allocated by a deleted item
fn free_deleted_item(item: &mut DbItem) {
    let _lock = item.mtx.lock();
    if !item.has_writer {
        item.chunks = Vec::new();
        item.name = String::new();
        item.for_deletion = false;
    }
}

// called by DELETE, initiates deletion process of a file
pub async fn delete_item<'a>(name: String) -> StatusCode {
    let hashed_names = get_name_hashes(&name);
    for mut hashed_name in hashed_names {
        hashed_name %= DB_LENGTH;
        unsafe {
            if DB[hashed_name].name == name && !DB[hashed_name].for_deletion {
                mark_for_deletion(&mut DB[hashed_name]);
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
                    interval.tick().await; // completes instantly
                    interval.tick().await; // give time to readers and writer to finish
                    free_deleted_item(&mut DB[hashed_name]); // should be safe to delete now
                });
                return StatusCode::ACCEPTED;
            }
        }
    }
    StatusCode::NOT_FOUND
}

// tries to find a place in the hash table for a PUT request
fn get_writable_id(name: String) -> (usize, bool) {
    let hashed_names = get_name_hashes(&name);
    unsafe {
        for mut hashed_name in hashed_names {
            hashed_name %= DB_LENGTH;
            let myfile = &mut DB[hashed_name];
            let _lock = myfile.mtx.lock();
            if myfile.name.len() == 0 || myfile.name == name {
                if myfile.has_writer {
                    return (usize::MAX, true);
                }
                myfile.has_writer = true;
                myfile.name = name;
                return (hashed_name, myfile.name.len() > 0);
            }
        }
    }
    (usize::MAX, false)
}

// wakes up all waiters in the queue for the current item
fn wake_all_waiters(item: &mut DbItem) {
    let _lock = item.mtx.lock();
    item.waking = true;
    let queue = &mut item.waiters;
    loop {
        match queue.front() {
            None => {
                break;
            }
            Some(waker) => {
                waker.wake_by_ref();
                queue.pop_front();
            }
        }
    }
    item.waking = false;
}

// wakes up the first waiter for an item, which will then proceed to wake others from the queue
fn wake_first_waiter(waiter: &Option<Waker>) {
    match waiter {
        None => {
            // println!("This shouldn't happen.") // though it somehow did once, shouldn't be an issue tho
        }
        Some(waker) => {
            waker.wake_by_ref();
        }
    }
}

// finds a part of a chunk suitable for writing the incoming data
fn get_writable_chunk(myfile: &mut DbItem) -> &mut [u8] {
    let write_chunk = &mut myfile.chunks[myfile.len / CHUNK_SIZE];
    if write_chunk.len() == 0 {
        write_chunk.resize(CHUNK_SIZE, 0);
    }
    let start = myfile.len % CHUNK_SIZE;
    let mut end = start + BUFFERED_LEN;
    if end > CHUNK_SIZE {
        end = CHUNK_SIZE;
    }
    &mut write_chunk[start..end]
}

// fulfills a PUT request if possible
pub async unsafe fn from_stream_to_item(stream: BodyDataStream, name: String, content_type: HeaderValue) -> StatusCode {
    // first initialize some stuff
    let (id, exists) = get_writable_id(name);
    if id == usize::MAX {
        if exists {
            return StatusCode::NOT_IMPLEMENTED; // multiple put to 1 file at the same time, possibly incorrect code for this situation?
        }
        else {
            return StatusCode::INSUFFICIENT_STORAGE;
        }
    }
    let myfile = &mut DB[id];
    myfile.content_type = content_type;
    let stream_with_err = stream.map_err(|err| io::Error::new(io::ErrorKind::Other, err));
    let mut reader = StreamReader::new(stream_with_err);
    if myfile.chunks.len() == 0 {
        myfile.chunks.resize(MAX_CHUNK_NUM, Vec::new());
    }

    // loop for writing incoming data
    while myfile.len != MAX_FILE_SIZE && !myfile.for_deletion {
        let write_chunk = get_writable_chunk(myfile);
        let read_len = reader.read(write_chunk).await.unwrap_or(0);
        myfile.len += read_len;

        if read_len == 0 { // incoming stream most likely ended
            break;
        }

        if myfile.wake_me { // a first waiter, first wake him then turn this false to allow new waiters to take its place
            wake_first_waiter(&myfile.waiter);
            myfile.wake_me = false;
        } else if !myfile.waking && !myfile.waiters.is_empty() {
            // someone probably didn't do their job, so let's do it, should be a relatively rare occurence
            // in testing with 32 writers and 128 readers per each at ~256Kbps occured around 5 times a second, so should be less than 2 % of the time
            // at the same time this should occur less with even more readers
            tokio::spawn(async move {
                wake_all_waiters(&mut DB[id]);
            });
        }
    }

    {
        let _lock = myfile.mtx.lock();
        if myfile.for_deletion {
            DB[id].len = 0;
        }
        myfile.has_writer = false;
    }
    wake_first_waiter(&myfile.waiter);
    wake_all_waiters(myfile); // in case first waiter aborted connection
    if exists {
        return StatusCode::NO_CONTENT;
    } else {
        return StatusCode::CREATED;
    }
}

// returns hashes for a file path
fn get_name_hashes(name: &String) -> Vec<usize> {
    let mut hasher = DefaultHasher::new();
    let mut hash_vec: Vec<usize> = Vec::new();
    hash_vec.reserve_exact(MAX_REHASH_NUM);
    let mut i = 1;
    name.hash(&mut hasher);
    hash_vec.push(hasher.finish() as usize);
    while i < MAX_REHASH_NUM {
        hash_vec[i - 1].hash(&mut hasher);
        hash_vec.push(hasher.finish() as usize);
        i += 1;
    }
    hash_vec
}

// a stream to facilitate synchronization of readers with a concurrent writer
pub struct DbItemStream<'a> {
    item: &'a DbItem, // reference to file item
    id: usize, // used for unsafe access to the file item
    start: usize, // where to start reading
    end: usize, // where to stop reading
    was_first_waiter: bool, // indicates this reader should wake other waiting readers of the same item
}

impl DbItemStream<'_> {
    pub fn new(item: &DbItem, id: usize, start: usize, end: usize) -> DbItemStream<'_> {
        DbItemStream {
            item: item,
            id: id,
            start: start,
            end: end,
            was_first_waiter: false,
        }
    }

    // in a bit of a clumsy way returns a readable chunk range
    fn get_read_range(&mut self) -> (usize, usize) {
        let start = self.start % CHUNK_SIZE;
        let mut end = start + BUFFERED_LEN;
        if end > CHUNK_SIZE {
            end = CHUNK_SIZE;
        }
        if end - start > self.end - self.start {
            end = start + self.end - self.start;
        }
        if end - start > self.item.len - self.start {
            end = start + self.item.len - self.start;
        }
        self.start += end - start;
        (start, end)
    }

    // enters a queue of waiters for a file or its first waiter
    fn enter_queue<'a>(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<&'a [u8], io::Error>>> {
        let _lock = self.item.mtx.lock();
        if !self.item.has_writer {
            // there could have been more data written while we were acquiring the lock and this should lead to a new call of poll_next
            return std::task::Poll::Ready(Some(Ok(&[]))); 
        }
        if self.item.for_deletion {
            return std::task::Poll::Ready(None);
        }
        unsafe {
            if DB[self.id].wake_me {
                // there is a first waiter
                DB[self.id].waiters.push_back(cx.waker().to_owned()); 
            } else {
                // become the first waiter
                DB[self.id].waiter = Some(cx.waker().to_owned());
                DB[self.id].wake_me = true;
                self.was_first_waiter = true;
            }
        }
        std::task::Poll::Pending
    }
}

impl<'a> Stream for DbItemStream<'a> {
    type Item = Result<&'a [u8], io::Error>;

    // polls if new data is available for reading
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        ctx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.was_first_waiter {
            // we were the first waiter, time to wake up others
            // this could be done with a spawned task but it is probably better to slow down 1 reader instead of all the others
            unsafe {
                wake_all_waiters(&mut DB[self.id]);
            }
            self.was_first_waiter = false;
        }
        if self.start != self.end && !self.item.for_deletion { // for_deletion check is not useless, setting len to 0 could be delayed
            if self.item.len > self.start {
                // return a readable chunk of data
                let chunk = &self.item.chunks[self.start / CHUNK_SIZE];
                let (start, end) = self.get_read_range();
                return std::task::Poll::Ready(Some(Ok(&chunk[start..end])));
            }
            if self.item.has_writer {
                return self.enter_queue(ctx);
            }
        }
        std::task::Poll::Ready(None)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.item.has_writer {
            return (self.item.len - self.start, None);
        } else {
            return (self.item.len - self.start, Some(self.item.len - self.start));
        }
    }
}
