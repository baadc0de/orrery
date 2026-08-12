# P0 NAT lab — node assignments (stable identities via --secret-key)
# Each node keeps a fixed NodeId across runs, so the mesh roster is stable.
#
# node  role        secret key (hex)                                   NodeId
# 0     host        b227c281…c4ba4                                     c29f444c…d0e5
# 1     gw-a p1    76b9b01e…e976                                     db0a064a…79e8
# 2     gw-a p2    bb34e38b…0f81                                     5f143f06…d4fc
# 3     gw-b p3    67dc9e23…a6cd                                     edc367eb…888f
# 4     gw-b p4    27a83845…d2ed6                                    5a25c7b8…d60f
# 5     gw-c p5    7657fb0b…1a48                                     1531d658…9fc6
# 6     gw-c p6    2653e939…8d2a                                     221a1a88…9239a
# 7     gw-d p7    7869fb9b…6191                                     c7de7101…7f7e
# 8     gw-d p8    bbde3cac…2bec                                     86523014…c24

SECRETS=(
"b227c281aeaf1355a25d1a600c440a88ea87f47244241d9a25ea09ee85ac4ba4"
"76b9b01e7e04eddbb428d764fc42edaca5e9b2ca40cbcccba809eab152a3e976"
"bb34e38bc5626ea39f7421ed3661214656e222e27c9720a6f88537a8d3f40f81"
"67dc9e23549f0d2e6c1f6d0b558f8fae8755083ecb5b6c666a23dea98f05a6cd"
"27a838454d862d2dbd7ecca3a598ac3225c16c45ddd145d7cd5c854f92dc2ed6"
"7657fb0bf4a43b197be5e953cc936062a85697a4dd7b2953cc24a61ce2b51a48"
"2653e939d6fba5122ccc92157a59543ddc879409e6cbd5866427d48878588d2a"
"7869fb9bd494f2da5196f3c06c67bb3fd9bab0ab2b5f31dd071b4a512bd76191"
"bbde3cac1686130c12011f7633a5fc87ad32fcd48d4957ae6b39f40bf6a02bec"
)