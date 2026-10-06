# Minimal guest initramfs for bedrock VM tests.
# The default init shuts down immediately; initSource supplies other test guests.
{ pkgs, initSource ? null }:

let
  initBin = pkgs.stdenv.mkDerivation {
    name = "bedrock-guest-init";
    dontUnpack = true;
    buildPhase = ''
      ${if initSource != null then "cp ${initSource} init.c" else ''
      cat > init.c << 'EOF'
      #include "libvmcall.h"
      void _start(void) {
          vmcall_shutdown();
          for (;;) __asm__ volatile("hlt");
      }
      EOF
      ''}
      $CC -I${../guest} -static -nostdlib -o init init.c
    '';
    installPhase = "cp init $out";
  };
in
pkgs.runCommand "bedrock-guest-rootfs" {
  nativeBuildInputs = [ pkgs.cpio pkgs.gzip ];
} ''
  mkdir -p root
  cp ${initBin} root/init
  chmod +x root/init
  cd root
  find . -print0 | cpio --null -o -H newc | gzip -9 > $out
''
