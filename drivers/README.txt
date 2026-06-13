Staging area for unmodified Microsoft kernel drivers (.sys).

Drop a real driver here to have the kernel load it at boot against our
ntoskrnl.exe export table, the same way winbin/ holds real user binaries.

  null.sys   The Windows NULL device driver. Copy it from a Windows machine:
               C:\Windows\System32\drivers\null.sys
             then place it here as drivers/null.sys and rebuild the kernel.

When present, the boot self tests load it via ldr_load_driver (running its
DriverEntry), then exercise \Device\Null (a write should consume all bytes,
a read should return end-of-file). The kernel build embeds the file via
build.rs (NTOS_NULL_SYS_IMAGE); when absent, the test is skipped and the
build is unaffected.

If the loader logs "unresolved import <Name>" for the driver, that ntoskrnl
routine needs adding to kernel/src/ldr/exports.rs.
