# The built-in `ubuntu-26.04` runner for `bosn ci run` (act2). catthehacker
# publishes no act-26.04 image, so bosn builds this one inside the engine and
# records it by the sha256 of this file. Everything it starts from is pinned:
# the base by manifest digest (linux/amd64), every apt package by an Ubuntu
# archive snapshot, and Node by the release tarball's sha256.
FROM docker.io/library/ubuntu@sha256:88a381d5b5eeb2b35d3ad70925a362c37ce569daf43ede89ff818ec20e4d3794

ARG UBUNTU_SNAPSHOT=20261015T000000Z
ARG NODE_VERSION=24.19.0
ARG NODE_SHA256=14b342e71204f811bde6153be8e04b62aef63c236fef92b55f9c83154b409647

ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 ImageOS=ubuntu26

# What the catthehacker act image gives the actions jobs rely on: git (and
# LFS) for actions/checkout, zstd/tar/gzip for actions/cache, curl/wget, sudo,
# python3, a C toolchain, jq, unzip/zip, ssh and gnupg. The snapshot service
# is HTTPS-only and the base image has no CA bundle, so ca-certificates comes
# first from the signed live archive; the snapshot then pins every package.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && apt-get update --snapshot "$UBUNTU_SNAPSHOT" \
 && apt-get install -y --no-install-recommends --snapshot "$UBUNTU_SNAPSHOT" \
      build-essential ca-certificates curl file git git-lfs gnupg gzip jq \
      locales lsb-release openssh-client python3 python3-pip python3-venv \
      rsync sudo tar tzdata unzip wget xz-utils zip zstd \
 && apt-get clean \
 && find /var/lib/apt/lists -mindepth 1 -delete

# Node for JavaScript actions, where the default runner keeps it, so the
# PATH bosn passes act names it on every runner.
RUN curl -fsSLo /tmp/node.tar.xz "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
 && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
 && mkdir -p "/opt/acttoolcache/node/${NODE_VERSION}/x64" \
 && tar -xJf /tmp/node.tar.xz --strip-components=1 -C "/opt/acttoolcache/node/${NODE_VERSION}/x64" \
 && find /tmp -name node.tar.xz -delete

ENV PATH=/opt/acttoolcache/node/24.19.0/x64/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN node --version && git --version && python3 --version && zstd --version
