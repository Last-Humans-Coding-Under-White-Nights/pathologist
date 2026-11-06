using NineCallback = void(*)();
void alpha();
NineCallback nine_remote=alpha;
NineCallback &nine_free_get() { return nine_remote; }
