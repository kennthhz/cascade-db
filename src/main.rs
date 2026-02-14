use aquifer::{PageId, UringStorage, PAGE_SIZE};
use aquifer::storage::AlignedBuf;

fn main() {
    tokio_uring::start(async {
        let data_dir = "./cascade_data";
        std::fs::create_dir_all(data_dir).ok();
        let storage = UringStorage::new(data_dir);

        println!("--- 🛠️ CASCADE DB NATIVE LINUX SMOKE TEST ---");

        let mut write_buf = AlignedBuf::new(PAGE_SIZE);

        // Fill page payload (skip checksum bytes [0..4])
        {
            let s = write_buf.as_mut_total_slice();
            for i in 4..PAGE_SIZE {
                s[i] = (i % 255) as u8;
            }
        }

        // Mark bytes as initialized so checksum calculation doesn't touch uninitialized memory
        write_buf.set_init_len(PAGE_SIZE);

        let pid = PageId {
            space_id: 1,
            page_no: 88,
        };

        println!("💾 Writing Page 88 (Direct I/O)...");
        let (res, write_buf) = storage.write_page(pid, write_buf).await;
        res.expect("Write failed");

        println!("🔍 Reading Page 88 and Verifying...");
        let read_buf_raw = AlignedBuf::new(PAGE_SIZE);
        let (res, read_buf) = storage.read_page(pid, read_buf_raw).await;
        res.expect("Read failed");

        assert_eq!(
            &write_buf.as_init_slice()[4..PAGE_SIZE],
            &read_buf.as_init_slice()[4..PAGE_SIZE]
        );

        println!("--- ✅ SMOKE TEST PASSED ---");
    });
}
