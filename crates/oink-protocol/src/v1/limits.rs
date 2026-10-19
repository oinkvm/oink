//! Fixed v1 ceilings. Frame limits include the six-byte envelope unless noted.
macro_rules! fixed_limits {
    ($($name:ident:$ty:ty=$value:expr;)*)=>{
        $(pub const $name:$ty=$value;)*
        pub const ALL:&[(&str,u64)]=&[$((stringify!($name),$name as u64),)*];
    };
}
fixed_limits! {
    RELIABLE_FRAME:usize=16 * 1024;
    CONFIG_FRAME:usize=128 * 1024;
    LOW_FRAME:usize=16 * 1024 * 1024;
    DATAGRAM:usize=1100;
    DISCOVERY:usize=1200;
    CONFIG_STATE:usize=120 * 1024;
    LOW_CHUNK:usize=16 * 1024;
    LOW_OUTER_FRAME:usize=17 * 1024;
    MAP_KEYS:usize=65_535;
    DEPTH:usize=32;
    REGISTERS:usize=256;
    MEMBERS:usize=16;
    ROSTER_RECORDS:usize=256;
    SCREENS:usize=256;
    CAPABILITIES:usize=128;
    IDENTIFIER_BYTES:usize=96;
    PUBLIC_KEY_BYTES:usize=4096;
    SIGNATURE_BYTES:usize=1024;
    PAIR_OBJECT:usize=1024 * 1024;
    PAIR_OBJECTS_PER_LINK:usize=2;
    PAIR_STAGING:usize=4 * 1024 * 1024;
    PENDING_PAIRS:usize=3;
    ACTIVE_PAIRS:usize=32;
    PAIRS_PER_PEER:usize=4;
    PAIR_ATTEMPTS:u8=3;
    PAIR_PROGRESS_SECONDS:u64=10;
    INVENTORY:usize=8 * 1024 * 1024;
    REPRESENTATION:usize=256 * 1024 * 1024;
    SNAPSHOT:usize=1024 * 1024 * 1024;
    FILE_BLOCK:usize=1024 * 1024;
    DIRECTORY_ENTRIES:usize=1024;
    INLINE:usize=64 * 1024;
    RELIABLE_INLINE:usize=4 * 1024;
    AUTO_FETCH:usize=16 * 1024 * 1024;
    CONTENT_PROGRESS_SECONDS:u64=300;
    DIAGNOSTIC_SEGMENT:usize=512 * 1024;
    DIAGNOSTIC_COLLECTION:usize=64 * 1024 * 1024;
    DIAGNOSTIC_PROGRESS_SECONDS:u64=30;
    RESOURCE_FACTS:usize=8 * 1024;
    ERROR_RATE_PER_SECOND:u32=8;
    MAX_HEADER:usize=256;
    EXIT_VOLUME_STATE:usize=2 * 1024;
    LOCAL_PAUSE_STATE:usize=1024;
    OWNER_PREFERENCES:usize=32;
    CONTROL_FRAME:usize=4 * 1024;
    CONTROL_OUTSTANDING:usize=32;
    CONTROL_PER_REQUESTER:usize=4;
    CONTROL_CACHED_RESULTS:usize=256;
    CONTROL_DEADLINE_SECONDS:u64=10;
}
