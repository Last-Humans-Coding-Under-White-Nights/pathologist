using ConsumerCallback = void (*)();
void alpha(); void beta(); void unrelated();
void consumer_range(bool c) { ConsumerCallback a[1]={alpha}, b[1]={beta}; for (ConsumerCallback f:(c?a:b)) f(); }
void consumer_range_auto(bool c) { ConsumerCallback a[1]={alpha}, b[1]={beta}; for (auto f:(c?a:b)) f(); }
void consumer_range_reference(bool c) { ConsumerCallback a[1]={alpha}, b[1]={beta}; for (auto &f:(c?a:b)) f(); }
void consumer_range_comma() { ConsumerCallback a[1]={alpha}; for (ConsumerCallback f:(0,a)) f(); }
void consumer_range_control() { ConsumerCallback a[1]={alpha}; for (ConsumerCallback f:a) f(); }
struct ConsumerFun { void operator()() {} };
void consumer_callable(bool c) { ConsumerFun a,b; (c?a:b)(); }
void consumer_callable_refs(bool c, ConsumerFun &a, ConsumerFun &b) { (c?a:b)(); }
void consumer_callable_comma(ConsumerFun &a) { (0,a)(); }
void consumer_callable_assignment(ConsumerFun &a, ConsumerFun &b) { (a=b)(); }
void consumer_callable_control() { ConsumerFun a; a(); }
struct ConsumerRun { void run() {} };
void consumer_comma_receiver(ConsumerRun *p) { (0,p)->run(); }
void consumer_assignment_receiver(ConsumerRun *p) { ConsumerRun *q; (q=p)->run(); }
void consumer_comma_object(ConsumerRun &p) { (0,p).run(); }
void consumer_assignment_object(ConsumerRun &p, ConsumerRun &q) { (q=p).run(); }
void consumer_mixed_array(bool c, ConsumerRun *p) { ConsumerRun a[1]; (c?a:p)->run(); }
void consumer_mixed_array_reverse(bool c, ConsumerRun *p) { ConsumerRun a[1]; (c?p:a)->run(); }
struct ConsumerOps { ConsumerCallback f; };
void consumer_mixed_field(bool c) {
    ConsumerOps a[1]={{alpha}}, b{beta}; ConsumerOps *p=&b; (c?a:p)->f();
}
ConsumerCallback consumer_offset_table[1]={alpha};
ConsumerCallback &consumer_offset_get(int i) { return *(consumer_offset_table+i); }
ConsumerCallback &consumer_offset_reverse_get(int i) { return *(i+consumer_offset_table); }
ConsumerCallback &consumer_offset_control_get(int i) { return consumer_offset_table[i]; }
void consumer_offset_invoke(ConsumerCallback *p) { (*p)(); }
void consumer_offset_use() { consumer_offset_invoke(&consumer_offset_get(0)); }
void consumer_offset_reverse_invoke(ConsumerCallback *p) { (*p)(); }
void consumer_offset_reverse_use() { consumer_offset_reverse_invoke(&consumer_offset_reverse_get(0)); }
void consumer_offset_control_invoke(ConsumerCallback *p) { (*p)(); }
void consumer_offset_control_use() { consumer_offset_control_invoke(&consumer_offset_control_get(0)); }
ConsumerCallback consumer_member_alpha=alpha, consumer_member_beta=beta, consumer_member_bad=unrelated;
struct ConsumerGetter {
    ConsumerCallback &get() { return consumer_member_alpha; }
    ConsumerCallback &get(int) { return consumer_member_bad; }
    ConsumerCallback &pick(int) { return consumer_member_alpha; }
    ConsumerCallback &pick(double) { return consumer_member_bad; }
};
ConsumerGetter consumer_member_object;
ConsumerCallback *consumer_member_address=&consumer_member_object.get();
extern bool consumer_condition;
ConsumerCallback *consumer_member_selection=consumer_condition?&consumer_member_object.get():&consumer_member_beta;
ConsumerCallback *consumer_member_typed=&consumer_member_object.pick(0);
void consumer_member_call() { (*consumer_member_address)(); }
void consumer_member_selected_call() { (*consumer_member_selection)(); }
void consumer_member_typed_call() { (*consumer_member_typed)(); }
ConsumerCallback &consumer_free_get() { return consumer_member_alpha; }
ConsumerCallback *consumer_free_address=&consumer_free_get();
void consumer_member_free_control() { (*consumer_free_address)(); }
struct ConsumerRemoteGetter { ConsumerCallback &get(); ConsumerCallback &pick(int); ConsumerCallback &pick(double); };
ConsumerCallback &ConsumerRemoteGetter::pick(double) { return consumer_member_bad; }
ConsumerRemoteGetter consumer_remote_object;
ConsumerCallback *consumer_remote_address=&consumer_remote_object.get();
void consumer_remote_call() { (*consumer_remote_address)(); }
ConsumerCallback *consumer_remote_typed_address=&consumer_remote_object.pick(0);
void consumer_remote_typed_call() { (*consumer_remote_typed_address)(); }
