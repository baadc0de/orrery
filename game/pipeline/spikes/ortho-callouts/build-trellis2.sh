#!/bin/bash
set -e
E=~/trellis2-env; P=$E/bin/python; X=~/trellis2-ext; L=~/trellis2-build.log
export CUDA_HOME=$E; export PATH=$E/bin:$PATH; export CC=$E/bin/x86_64-conda-linux-gnu-gcc; export CXX=$E/bin/x86_64-conda-linux-gnu-g++
export TORCH_CUDA_ARCH_LIST="8.9"; export MAX_JOBS=24; export CUDAHOSTCXX=$CXX
export LIBRARY_PATH=/usr/lib:$E/lib/stubs:$E/lib:$LIBRARY_PATH; export LDFLAGS="-L/usr/lib -L$E/lib/stubs -L$E/lib"; export TMPDIR=$HOME/tmp
step(){ echo "=== $1 $(date +%T)" | tee -a $L; }
step torch; $P -m pip install -q torch==2.6.0 torchvision==0.21.0 --index-url https://download.pytorch.org/whl/cu124 >> $L 2>&1
step basic; $P -m pip install -q imageio imageio-ffmpeg tqdm easydict opencv-python-headless ninja trimesh transformers tensorboard pandas lpips zstandard pillow kornia timm rembg onnxruntime-gpu >> $L 2>&1
$P -m pip install -q git+https://github.com/EasternJournalist/utils3d.git@9a4eb15e4021b67b12c460c7057d642626897ec8 >> $L 2>&1
step flash-attn; $P -m pip install -q flash-attn==2.7.3 --no-build-isolation >> $L 2>&1 || { echo "flash-attn prebuilt failed; will use xformers" | tee -a $L; $P -m pip install -q xformers --index-url https://download.pytorch.org/whl/cu124 >> $L 2>&1; }
step nvdiffrast; $P -m pip install -q $X/nvdiffrast --no-build-isolation >> $L 2>&1
step nvdiffrec; $P -m pip install -q $X/nvdiffrec --no-build-isolation >> $L 2>&1
step cumesh; $P -m pip install -q $X/CuMesh --no-build-isolation >> $L 2>&1
step flexgemm; $P -m pip install -q $X/FlexGEMM --no-build-isolation >> $L 2>&1
step o-voxel; rm -rf $X/o-voxel; cp -r ~/TRELLIS.2/o-voxel $X/o-voxel; $P -m pip install -q $X/o-voxel --no-build-isolation >> $L 2>&1
step verify; $P -c "import torch, nvdiffrast, flexgemm, cumesh, o_voxel; print('torch', torch.__version__, torch.cuda.is_available()); import importlib; print('attn', 'flash_attn' if importlib.util.find_spec('flash_attn') else 'xformers')" 2>&1 | tee -a $L
step done
