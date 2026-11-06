#pragma once
namespace std {
template<bool, class T = void> using enable_if_t = T;
}
// Reductions of HiView encoded_param.h's template heads and instantiations.
#define SFINAE std::enable_if_t<true>* = nullptr
template<class T, SFINAE>
struct EncodedParam {
    void run(int value = 7) { helper(); }
    void helper() {}
};
extern template class EncodedParam<int>;
template struct EncodedParam<long>;
namespace ns {
template<class T> struct Box {
    T *data();
    void run(T) {}
    template<class U> U convert(U);
};
}
extern template int *ns::Box<int>::data();
extern template void ns::Box<int>::run(int);
template void ns::Box<long>::run(long);
extern template int ns::Box<int>::convert<int>(int);
// Reductions of raw_data_builder.h and sys_event.h member instantiations.
struct RawDataBuilder { template<class T> void Append(T value) {} };
extern template void RawDataBuilder::Append<int>(int);
struct SysEvent { template<class T> static void SetEventValue(T value) {} };
extern template void SysEvent::SetEventValue<const char *>(const char *);
// Nested defaults and reference-returning/member-template instantiations.
template<class T, std::enable_if_t<true, std::enable_if_t<true>>* = nullptr, int N = 3>
struct NestedEnabled { void run(int value = 9) {} };
template<class K, class V> struct Map {};
template<class F> struct Fn {};
struct Builder {
    template<class T> Builder &AppendValue(const char *key, T value) { return *this; }
    template<class T> void Init(Map<int, Fn<Builder &(T)>> &handlers) {}
};
extern template Builder &Builder::AppendValue<double>(const char *, double);
template Builder &Builder::AppendValue<long>(const char *, long);
extern template void Builder::Init<const char *>(Map<int, Fn<Builder &(const char *)>> &);
