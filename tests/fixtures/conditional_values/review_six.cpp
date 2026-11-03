using SixF = void(*)();
void alpha(); void beta();
struct SixFirst { SixF a; SixF b; };
SixFirst six_table[1] = {{alpha,beta}};
SixF *six_first_get() { return reinterpret_cast<SixF*>(six_table); }
void six_first_returned() { SixF *p=six_first_get(); SixF f=*p; f(); }
void six_first_use(SixF *p) { (*p)(); }
void six_first_argument() { six_first_use(reinterpret_cast<SixF*>(six_table)); }
void six_first_callee() { (*reinterpret_cast<SixF*>(six_table))(); }
void six_first_read() { SixF f=*reinterpret_cast<SixF*>(six_table); f(); }
SixF *six_first_nested_get() { return static_cast<SixF*>(reinterpret_cast<SixF*>(six_table)); }
void six_first_nested_returned() { SixF f=*six_first_nested_get(); f(); }
void six_first_other() { six_table[0].b(); }
SixF six_default_make() { return alpha; }
void six_default_take(const SixF &f=six_default_make()) { f(); }
void six_default_caller() { six_default_take(); }
struct SixDefaultMember { void take(const SixF &f=six_default_make()) { f(); } };
void six_default_member_caller() { SixDefaultMember s; s.take(); }
struct SixDefaultConstruct { SixDefaultConstruct(const SixF &f=six_default_make()) { f(); } };
void six_default_construct_caller() { SixDefaultConstruct s; }
using SixStart = void*(*)(void*);
int pthread_create(void*,void*,SixStart,void*);
void *six_default_entry(void*) { return nullptr; }
void six_default_thread(int = pthread_create(nullptr,nullptr,six_default_entry,nullptr)) {}
void six_default_loop(int n) { six_default_thread(); for(int i=0;i<n;++i) six_default_thread(); }
void six_default_once() { six_default_thread(); }
void six_default_chain(int = (six_default_thread(),0)) {}
void six_default_chain_loop(int n) { six_default_chain(); for(int i=0;i<n;++i) six_default_chain(); }
void six_default_before() {} void six_default_after() {}
void six_default_sequence() { six_default_before(); six_default_take(); six_default_after(); }
