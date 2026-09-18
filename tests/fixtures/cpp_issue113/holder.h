#pragma once
namespace app {
template<class T> struct Holder { void Ping() {} T Get() { return value; } T value; };
}
