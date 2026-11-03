using Callback = void (*)();
void alpha();
static Callback remote_callback = alpha;
Callback &remote_reference() { return remote_callback; }
