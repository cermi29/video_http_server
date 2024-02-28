
# Simple Live Video HTTP Server

This is a simple HTTP server specialized for GET and PUT requests of live video segments.

## Key Design Choice

Assuming 1 writer and a big number of readers per 1 file, emphasis was put on writer not acquiring any mutex if possible as to minimize possible delay. My testing showed task spawning (and therefore likely acquiring a mutex) to wake up all waiting readers happened in less than 2% of write loops. Normally, 1 reader is signalled without requiring a mutex. This reader then proceeds to wake others.

## Working Features

- PUT and GET requests, always with Transfer-Encoding: chunked. 
- Data available to read instantly after copying from an intermediate buffer of a predefined max size (defined as a const, default 32KB).
- DELETE requests instantly prevent reading, even during an ongoing PUT, except for already ongoing read/write of 1 chunk (again max 32KB default).
- LIST where URL = prefix, lists paths with prefix. Sends a simple text file, 1 path on each line. Root URL `/` returns all files.

## Limitations and/or Issues

- No command line settings, important configurations are currently hard-coded in as a const value at the beginning of each source file. Default address for listen is `0.0.0.0:3000`.
- GET accepts Range header only in a specific format: `bytes=X-Y` where `X` and `Y` are a number or an empty string. Ignores any other headers. Always uses `Transfer-Encoding: chunked`.
- PUT assumes a linear input from the encoder, i.e. allows one concurrent writer per file and ignores Content-Range header – simply appends to existing file if it exists. If there is currently another writer returns `501 Not Implemented` – questionable. Multiple writers could be implemented in this way:
  - Track the number of writers instead of just whether a writer exists
  - Change it so each chunk in memory is separate (currently several chunks are merged to simplify allocation).
  - Mutexing writes to each chunk to avoid writing to the same one due to a range being in the middle of one or even overlapping due to conflicting ranges (which is probably undefined behaviour anyway).
  - A waiting queue of readers for each chunk. This would require a common mutex for read and writing also for each chunk, to make sure no reader will go back to queue after we wake everyone. So maybe sticking to a global waiting queue and just going back to wait if data is still not available would end up being a better choice in general.
- Doesn't verify authority to PUT or DELETE.
- DELETE makes the file inaccessible right away (except for already ongoing chunk read/writes), but waits ~5 seconds before deleting it from memory – to wait for all ongoing readers and writers to finish. That could of course lead to some wrong memory access if some readers hang. The simple fix would be to (through atomic operations) keep the number of current active readers and writers and have the last of them delete the file.
- LIST is just implemented as `BTreeSet` lookup for names and then concatting them, without providing any useful info e.g. size, type.
- Global variable used for the database of files (`Vec`). Reason: Inexperience with rust, I had trouble figuring out another way to mutably access a shared hash table of files without a mutex needed for every read/write.
- The server can only hold a `DB_LENGTH` amount of files. PUT will fail and return `507 Insufficient Storage` if it doesn't find a place for the file in hash table in `MAX_REHASH_NUM` different places. Some form of popping old and unused files from memory and saving/loading them to disk if needed would need to be implemented.
- Path for files is treated as a whole therefore what would normally be a directory could simultaneously be a file here. 
- A file could get deleted during a GET request after getting its ID and before starting the read. Then if a new PUT would miraculously make it in before the read starts AND it would also be a file with a different path having the same hash, would GET start reading a different file. A simple fix would probably be doing 1 more name check when the read starts. Though this is nigh impossible to happen.
- Possibly some nasty rust malpractices and hard to read code considering I had no prior knowledge of rust.

## Time Investment

- 30–40 hours of mostly learning rust through programming this and a program for testing it, studying `axum`, `tokio` and other docs, fighting the borrow checker, some testing. The final program ended up basically as I envisioned it at the start with just minor changes.
- 1 hour of studying some of the relevant HTTP specification.
- Basically no debugging, just 3 small issues overall that I solved in less than 5 minutes.

## Testing

32 concurrent PUT requests at average 256 KBps, each 128 active readers, some manual connection abortions and other tricky stuff I could think of.
