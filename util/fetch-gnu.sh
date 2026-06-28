#!/bin/bash -e
# This file is part of the uutils awk package.
#
# For the full copyright and license information, please view the LICENSE
# file that was distributed with this source code.
#
# Download and extract the upstream GNU awk (gawk) release tarball into the
# current directory. Run it from an (empty) directory that will hold the gawk
# tree, e.g.:
#
#   mkdir -p ../gnu.awk && (cd ../gnu.awk && bash ../awk/util/fetch-gnu.sh)
#
# The extracted tree ships gawk's own testsuite under test/ (a GPL Makefile.am
# plus the .awk programs, .in inputs and .ok expected outputs). We never copy
# that tree into our repo; util/run-gnu-testsuite.sh drives gawk's own
# `make check` against the Rust awk binary, fetched fresh at test time.
ver="5.3.2"
curl -L "https://ftp.gnu.org/gnu/gawk/gawk-${ver}.tar.xz" | tar --strip-components=1 -xJf -
