
# rlottie

[![Build Status](https://travis-ci.org/Samsung/rlottie.svg?branch=master)](https://travis-ci.org/Samsung/rlottie)
[![Build status](https://ci.appveyor.com/api/projects/status/n3xonxk1ooo6s7nr?svg=true&passingText=windows%20-%20passing)](https://ci.appveyor.com/project/smohantty/rlottie-mliua)


rlottie is a platform independent standalone c++ library for rendering vector based animations and art in realtime.

Lottie loads and renders animations and vectors exported in the bodymovin JSON format. Bodymovin JSON can be created and exported from After Effects with [bodymovin](https://github.com/bodymovin/bodymovin), Sketch with [Lottie Sketch Export](https://github.com/buba447/Lottie-Sketch-Export), and from [Haiku](https://www.haiku.ai).

For the first time, designers can create and ship beautiful animations without an engineer painstakingly recreating it by hand. Since the animation is backed by JSON they are extremely small in size but can be large in complexity!

Here are small samples of the power of Lottie.

![Example1](.Gifs/1.gif)
![Example2](.Gifs/2.gif)
![Example2](https://github.com/airbnb/lottie-ios/blob/master/_Gifs/Examples1.gif)

## Contents
- [Building Lottie](#building-lottie)
	- [Meson Build](#meson-build)
	- [Cmake Build](#cmake-build)
	- [Test](#test)
- [Demo](#demo)
- [Previewing Lottie JSON Files](#previewing-lottie-json-files)
- [Quick Start](#quick-start)
- [Dynamic Property](#dynamic-property)
- [Supported After Effects Features](#supported-after-effects-features)
- [Issues or Feature Requests?](#issues-or-feature-requests)

## Building Lottie
rottie supports [meson](https://mesonbuild.com/) and [cmake](https://cmake.org/) build system. rottie is written in `C++14`. and has a public header dependancy of `C++11`

### Meson Build
install [meson](http://mesonbuild.com/Getting-meson.html) and [ninja](https://ninja-build.org/) if not already installed.

Run meson to configure rlottie
```
meson build
```
Run ninja to build rlottie
```
ninja -C build
```

### Cmake Build

Install [cmake](https://cmake.org/download/) if not already installed

Create a build directory for out of source build
```
mkdir build
```
Run cmake command inside build directory to configure rlottie.
```
cd build
cmake ..

# install in a different path. eg ~/test/usr/lib

cmake -DCMAKE_INSTALL_PREFIX=~/test ..

```
Run make to build rlottie

```
make -j 2
```
To install rlottie library

```
make install
```

### Test

Configure to build test
```
meson configure -Dtest=true
```
Build test suit

```
ninja
```
Run test suit
```
ninja test
```
[Back to contents](#contents)

#
## Demo
If you want to see rlottie librray in action without building it please visit [rlottie online viewer](http://rlottie.com)

While building rlottie library it generates a simple lottie to GIF converter which can be used to convert lottie json file to GIF file.

Run Demo
```
lottie2gif [lottie file name]
```

#
## Previewing Lottie JSON Files
Please visit [rlottie online viewer](http://rlottie.com)

[rlottie online viewer](http://rlottie.com) uses rlottie wasm library to render the resource locally in your browser. To test your JSON resource drag and drop it to the browser window.

#
## Quick Start

Lottie loads and renders animations and vectors exported in the bodymovin JSON format. Bodymovin JSON can be created and exported from After Effects with [bodymovin](https://github.com/bodymovin/bodymovin), Sketch with [Lottie Sketch Export](https://github.com/buba447/Lottie-Sketch-Export), and from [Haiku](https://www.haiku.ai).

You can quickly load a Lottie animation with:
```cpp
std::unique_ptr<rlottie::Animation> animation =
					rlottie::loadFromFile(std::string("absolute_path/test.json"));
```
You can load a lottie animation from raw data with:
```cpp
std::unique_ptr<rlottie::Animation> animation = rlottie::loadFromData(std::string(rawData),
                                                                      std::string(cacheKey));
```

Properties like `frameRate` , `totalFrame` , `duration` can be queried with:
```cpp
# get the frame rate of the resource.
double frameRate = animation->frameRate();

#get total frame that exists in the resource
size_t totalFrame = animation->totalFrame();

#get total animation duration in sec for the resource
double duration = animation->duration();
```
Render a particular frame in a surface buffer `immediately` with:
```cpp
rlottie::Surface surface(buffer, width , height , stride);
animation->renderSync(frameNo, surface);
```
Render a particular frame in a surface buffer `asyncronousely` with:
```cpp
rlottie::Surface surface(buffer, width , height , stride);
# give a render request
std::future<rlottie::Surface> handle = animation->render(frameNo, surface);
...
#when the render data is needed
rlottie::Surface surface = handle.get();
```

[Back to contents](#contents)

#
## Dynamic Property

Update me.

#
## Supported After Effects Features

| **Shapes** | **Supported** |
|:--|:-:|
| Shape | 👍 |
| Ellipse | 👍 |
| Rectangle | 👍 |
| Rounded Rectangle | 👍 |
| Polystar | 👍 |
| Group | 👍 |
| Trim Path (individually) | 👍 |
| Trim Path (simultaneously) | 👍 |
| **Renderable** | **Supported** |
| Fill  | 👍 |
| Stroke | 👍 |
| Radial Gradient | 👍 |
| Linear Gradient | 👍 |
| Gradient Stroke | 👍 |
| **Transforms** | **Supported** |
| Position | 👍 |
| Position (separated X/Y) | 👍 |
| Scale | 👍 |
| Skew | ⛔️ |
| Rotation | 👍 |
| Anchor Point | 👍 |
| Opacity | 👍 |
| Parenting | 👍 |
| Auto Orient | 👍 |
| **Interpolation** | **Supported** |
| Linear Interpolation | 👍 |
| Bezier Interpolation | 👍 |
| Hold Interpolation | 👍 |
| Spatial Bezier Interpolation | 👍 |
| Rove Across Time | 👍 |
| **Masks** | **Supported** |
| Mask Path | 👍 |
| Mask Opacity | 👍 |
| Add | 👍 |
| Subtract | 👍 |
| Intersect | 👍 |
| Lighten | ⛔️ |
| Darken | ⛔️ |
| Difference | ⛔️ |
| Expansion | ⛔️ |
| Feather | ⛔️ |
| **Mattes** | **Supported** |
| Alpha Matte | 👍 |
| Alpha Inverted Matte | 👍 |
| Luma Matte | 👍 |
| Luma Inverted Matte | 👍 |
| **Merge Paths** | **Supported** |
| Merge | ⛔️ |
| Add | ⛔️ |
| Subtract | ⛔️ |
| Intersect | ⛔️ |
| Exclude Intersection | ⛔️ |
| **Layer Effects** | **Supported** |
| Fill | ⛔️ |
| Stroke | ⛔️ |
| Tint | ⛔️ |
| Tritone | ⛔️ |
| Levels Individual Controls | ⛔️ |
| **Text** | **Supported** |
| Glyphs |  ⛔️ |
| Fonts | ⛔️ |
| Transform | ⛔️ |
| Fill | ⛔️ |
| Stroke | ⛔️ |
| Tracking | ⛔️ |
| Anchor point grouping | ⛔️ |
| Text Path | ⛔️ |
| Per-character 3D | ⛔️ |
| Range selector (Units) | ⛔️ |
| Range selector (Based on) | ⛔️ |
| Range selector (Amount) | ⛔️ |
| Range selector (Shape) | ⛔️ |
| Range selector (Ease High) | ⛔️ |
| Range selector (Ease Low)  | ⛔️ |
| Range selector (Randomize order) | ⛔️ |
| expression selector | ⛔️ |
| **Other** | **Supported** |
| Expressions | ⛔️ |
| Images | 👍 |
| Precomps | 👍 |
| Time Stretch |  👍 |
| Time remap |  👍 |
| Markers | ⛔️ |

#
[Back to contents](#contents)

## Issues or Feature Requests?
File github issues for anything that is broken. Be sure to check the [list of supported features](#supported-after-effects-features) before submitting.  If an animation is not working, please attach the After Effects file to your issue. Debugging without the original can be very difficult. For immidiate assistant or support please reach us in [Gitter](https://gitter.im/rLottie-dev/community#)
