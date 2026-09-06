use esp_idf_part::{PartitionTable, Type};

#[test]
fn espflash_parser_accepts_scan_partition_and_csv_matches_binary() {
    let binary = include_bytes!("../../partitions.bin");
    let table = PartitionTable::try_from(binary.to_vec()).unwrap();
    table.validate().unwrap();
    let scan = table.find("keyscan").unwrap();
    assert_eq!(scan.ty(), Type::Custom(0x40));
    assert_eq!(u8::from(scan.subtype()), 0);
    assert_eq!(scan.offset(), 0x3f0000);
    assert_eq!(scan.size(), 0x10000);
    assert_eq!(table.to_bin().unwrap(), binary);

    let csv = PartitionTable::try_from(include_str!("../../partitions.csv")).unwrap();
    assert_eq!(csv.to_bin().unwrap(), binary);
}
