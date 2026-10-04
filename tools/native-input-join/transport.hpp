#pragma once
// Isolated fixture transport. No queue, reconnect, retry, action dispatch or admission API.
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <cerrno>
#include <cstdio>
#include <cstdint>
#include <fstream>
#include <sstream>
#include <string>
#include <stdexcept>
#include <limits>

namespace join_fixture {
inline std::string readBounded(int fd, size_t limit) {
 std::string out; char b[256];
 for (;;) {ssize_t n=read(fd,b,sizeof b);if(n==0)return out;if(n<0||out.size()+n>limit)throw std::runtime_error("bounded read");out.append(b,n);}
}
inline uint64_t startTicks(int proc) {
 int fd=openat(proc,"stat",O_RDONLY|O_CLOEXEC|O_NOFOLLOW);if(fd<0)throw std::runtime_error("stat open");
 std::string s;try{s=readBounded(fd,8192);}catch(...){close(fd);throw;}close(fd);
 auto end=s.rfind(')');if(end==std::string::npos)throw std::runtime_error("stat syntax");std::istringstream words(s.substr(end+1));std::string item;
 for(unsigned i=0;i<=19;i++)if(!(words>>item))throw std::runtime_error("stat fields");
 size_t used=0;auto ticks=std::stoull(item,&used);if(used!=item.size())throw std::runtime_error("stat start");return ticks;
}
inline uint64_t bootMs() {timespec ts{};if(clock_gettime(CLOCK_BOOTTIME,&ts)||ts.tv_sec<0)throw std::runtime_error("clock");return uint64_t(ts.tv_sec)*1000+ts.tv_nsec/1000000;}
class Sender {
 int socket_=-1,pidfd_=-1,proc_=-1,exe_=-1;
 uint64_t start_=0,sequence_=0;std::string prefix_;struct stat identity_{};bool dead_=false;
 bool alive() {
  pollfd p{pidfd_,POLLIN,0};if(poll(&p,1,0)!=0)return false;
  try{if(startTicks(proc_)!=start_)return false;}catch(...){return false;}
  struct stat current{};if(fstatat(proc_,"exe",&current,0)||current.st_dev!=identity_.st_dev||current.st_ino!=identity_.st_ino)return false;
  p.revents=0;return poll(&p,1,0)==0;
 }
 public:
 Sender(int fd,pid_t pid,uint64_t start,std::string prefix):socket_(fd),start_(start),prefix_(std::move(prefix)) {
  try {
   ucred peer{};socklen_t len=sizeof peer;if(getsockopt(fd,SOL_SOCKET,SO_PEERCRED,&peer,&len)||len!=sizeof peer||peer.pid!=pid||peer.uid!=getuid())throw std::runtime_error("peer identity");
   pidfd_=syscall(SYS_pidfd_open,pid,0);proc_=open(("/proc/"+std::to_string(pid)).c_str(),O_PATH|O_DIRECTORY|O_CLOEXEC|O_NOFOLLOW);
   if(pidfd_<0||proc_<0)throw std::runtime_error("pin peer");
   exe_=openat(proc_,"exe",O_PATH|O_CLOEXEC);if(exe_<0||fstat(exe_,&identity_)||!alive()||prefix_.empty()||prefix_.size()>1800||prefix_[0]!='{'||prefix_.find('\n')!=std::string::npos)throw std::runtime_error("peer admission");
  }catch(...){stop();if(pidfd_>=0)close(pidfd_);if(proc_>=0)close(proc_);if(exe_>=0)close(exe_);throw;}
 }
 Sender(const Sender&)=delete;Sender& operator=(const Sender&)=delete;
 ~Sender(){stop();if(pidfd_>=0)close(pidfd_);if(proc_>=0)close(proc_);if(exe_>=0)close(exe_);}
 void stop(){dead_=true;if(socket_>=0){shutdown(socket_,SHUT_RDWR);close(socket_);socket_=-1;}}
 bool edge(const char* kind,bool held=false) {
  if(dead_||!alive()||sequence_>=128){stop();return false;}
  try{
   auto stamp=bootMs();fprintf(stderr,"join-send edge=%s sequence=%llu boot_ms=%llu\n",kind,static_cast<unsigned long long>(sequence_+1),static_cast<unsigned long long>(stamp));
   auto msg=prefix_+",\"sequence\":\""+std::to_string(++sequence_)+"\",\"sent_ms\":\""+std::to_string(stamp)+"\",\"edge\":{\"kind\":\""+kind+"\"";
   if(std::string(kind)=="ready")msg+=held?",\"held\":true":",\"held\":false";
   msg+="}}\n";if(msg.size()>2049){stop();return false;}
   auto n=send(socket_,msg.data(),msg.size(),MSG_NOSIGNAL|MSG_DONTWAIT);
   if(n!=static_cast<ssize_t>(msg.size())||!alive()){stop();return false;}return true;
  }catch(...){stop();return false;}
 }
};
}
