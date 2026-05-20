JAVA_HOME ?= /data/data/com.termux/files/usr/lib/jvm/java-17-openjdk
CC = cc
LIBDIR = ../../target/debug

.PHONY: all test clean

all: TestTermuxSession.class libvproc_jni_bridge.so

TestTermuxSession.class: TestTermuxSession.java
	javac -h . TestTermuxSession.java

libvproc_jni_bridge.so: vproc_jni_bridge.c TestTermuxSession.class
	$(CC) -shared -fPIC \
		-I$(JAVA_HOME)/include \
		-I$(JAVA_HOME)/include/linux \
		vproc_jni_bridge.c \
		-L$(LIBDIR) -lvproc -lutil \
		-o libvproc_jni_bridge.so

test: all
	LD_LIBRARY_PATH=$(LIBDIR):. java -Djava.library.path=$(LIBDIR):. TestTermuxSession

clean:
	rm -f TestTermuxSession.class TestTermuxSession.h libvproc_jni_bridge.so
