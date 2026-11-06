using ConsumerCallback = void (*)();
void alpha();
ConsumerCallback consumer_remote_cell=alpha;
struct ConsumerRemoteGetter { ConsumerCallback &get(); ConsumerCallback &pick(int); ConsumerCallback &pick(double); };
ConsumerCallback &ConsumerRemoteGetter::get() { return consumer_remote_cell; }
ConsumerCallback &ConsumerRemoteGetter::pick(int) { return consumer_remote_cell; }
