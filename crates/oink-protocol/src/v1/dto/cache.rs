//! Cache accounting contains category counters, never paths or content hashes.
use super::*;
use crate::v1::schema::{no_validation, open_enum, wire_map};
open_enum!(CacheCategory {ClipboardFiles=0,DragFiles=1,ProviderCatalog=2,ProviderMaterialized=3,ProviderTemporary=4,DeliveredFiles=5,SourceSnapshots=6,PromiseExports=7,CompletedDeliveries=8});
wire_map!(CacheSize,no_validation,{
    1=>category:CacheCategory=required;2=>bytes:u64=required;3=>kept_bytes:u64=required;
});
wire_map!(CacheUsageValue,no_validation,{
    1=>categories:List<CacheSize,65535>=required;2=>total_bytes:u64=required;3=>incomplete:bool=required;
});
wire_map!(CacheClear,no_validation,{
    1=>removed_bytes:u64=required;2=>kept_bytes:u64=required;3=>usage:CacheUsageValue=required;
});
