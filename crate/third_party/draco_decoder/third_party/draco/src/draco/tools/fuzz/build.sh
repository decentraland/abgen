#!/bin/bash -eu

cmake $SRC/draco
make -j$(nproc)

for fuzzer in $(find $SRC/draco/src/draco/tools/fuzz -name '*.cc'); do
  fuzzer_basename=$(basename -s .cc $fuzzer)
  $CXX $CXXFLAGS \
    -I $SRC/ \
    -I $SRC/draco/src \
    -I $WORK/ \
    $LIB_FUZZING_ENGINE \
    $fuzzer \
    $WORK/libdraco.a \
    -o $OUT/$fuzzer_basename
done
