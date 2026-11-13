// mimalloc in place of the system allocator: much less time in malloc and free
// for view evaluation and the Location transform. musl's malloc takes one lock
// for all threads, which made the views many times slower in the Linux builds.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    fhir_pipes::main();
}
