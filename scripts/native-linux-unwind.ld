/* GNU release binaries abort on panic and do not expose native stack walking.
 * Keep the platform's default layout; remove only static native unwind tables.
 * Wasmtime's dynamically generated guest metadata is not a linker input. */
SECTIONS
{
  /DISCARD/ : { *(.eh_frame .eh_frame.*) }
}
INSERT AFTER .text;
