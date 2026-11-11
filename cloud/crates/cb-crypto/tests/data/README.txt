Test fixtures for cb-crypto.

TEST-ONLY KEYS. The key pair in this directory was generated solely for these
tests and has never been used for anything else. Do not use it for a project.

All files except this one were produced by the upstream BOINC `crypt_prog`
(lib/crypt_prog.cpp with lib/crypt.cpp, built against OpenSSL 3.5 on
Debian 13), so the tests check this crate against the reference
implementation that stock clients and servers use.

Build (from the repository root, with libssl-dev and g++ installed; config.h
is a hand-written file of HAVE_* defines for Linux, and stub.cpp defines
`const char* docker_cli_prog(DOCKER_TYPE) { return "docker"; }`, which
lib/util.cpp references):

    cd lib
    g++ -std=c++17 -I<dir with config.h> -I. -o crypt_prog \
        crypt_prog.cpp crypt.cpp md5_file.cpp md5.cpp filesys.cpp util.cpp \
        str_util.cpp parse.cpp miofile.cpp mfile.cpp stub.cpp -lssl -lcrypto

Commands:

    crypt_prog -genkey 1024 TEST_ONLY_private_key TEST_ONLY_public_key
    printf 'cb-crypto fixture file\nsecond line\n\000\001\002binary tail' \
        > sample_file.bin
    crypt_prog -sign sample_file.bin TEST_ONLY_private_key > sample_file.sig
    crypt_prog -sign_string 'hello BOINC' TEST_ONLY_private_key > hello.sig
    crypt_prog -sign_string \
        '<name>fixture_result_0_0</name><max_nbytes>5000000</max_nbytes>' \
        TEST_ONLY_private_key > upload_cert.sig

`crypt_prog -verify` and `-verify_string` accepted the signatures. In the
other direction, signatures made by this crate were written to files and
accepted by `crypt_prog -verify` / `-verify_string` when the fixtures were
made; tests/fixtures.rs asserts the stronger property that this crate's
signatures are byte-identical to the fixture signatures (the scheme is
deterministic).

MD5 of sample_file.bin: 57064a471b68e0deaf58405991432dd8
