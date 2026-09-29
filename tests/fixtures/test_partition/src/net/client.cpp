int SocketOpen(const char *host);
int FakeOpen();
int Connect() { return SocketOpen("a"); }
int ConnectFake() { return FakeOpen(); }
