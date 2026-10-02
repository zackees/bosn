# A NixOS-shaped root: the nixos/nix image has only /bin/sh and /usr/bin/env, no
# /lib64 and no /usr/lib -- the same gaps as a stock NixOS system.
FROM nixos/nix:2.35.2@sha256:7a007c766426c1877758ddc5cb87a965ac131fc78c582ce0083d922d51ae945c

RUN echo 'experimental-features = nix-command flakes' >> /etc/nix/nix.conf

# Native (Nix-built) tools the corpus drives. None of them is under test.
# A separate profile: the image's own profile already holds curl, gzip and friends.
RUN nix profile add --profile /tools \
      github:NixOS/nixpkgs/c59305bab2065cfecc4944690d9eedbb56f3a9fa#{curl,gnutar,gzip,xz,uv,gcc,file,patchelf,findutils,gnugrep,which}

ENV PATH=/tools/bin:$PATH

COPY layers.nix /fhs-src/layers.nix
RUN nix build --impure --file /fhs-src/layers.nix -o /fhs && ls -l /fhs/
