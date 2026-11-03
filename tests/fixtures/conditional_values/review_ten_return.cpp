using TenF = void(*)();
void alpha();
TenF ten_remote=alpha;
TenF &ten_overload_get() { return ten_remote; }
