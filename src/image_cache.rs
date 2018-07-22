use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;

use std::iter;

use glium;

use glium::texture::{RawImage2d, SrgbTexture2d};

use image;

pub mod errors {
    use glium::texture;
    use image;
    use std::io;

    error_chain!{
        foreign_links {
            Io(io::Error) #[doc = "Error during IO"];
            TextureCreationError(texture::TextureCreationError);
            ImageLoadError(image::ImageError);
        }
    }
}

use self::errors::*;

struct TextureLoader {
    curr_est_size: usize,

    running: Arc<AtomicBool>,
    remaining_capacity: isize,
    texture_cache: HashMap<PathBuf, CachedTexture>,
    join_handles: Option<Vec<thread::JoinHandle<()>>>,

    image_rx: Receiver<(PathBuf, fs::Metadata, image::RgbaImage)>,
    path_tx: Sender<PathBuf>,
}

impl TextureLoader {
    const MAX_BULK_PREFETCH_REQUEST: i32 = 4;

    /// # Arguemnts
    /// * `capacity` - Number of bytes. The last image loaded will be the one at which the allocated memory reaches or exceeds capacity
    pub fn new(capacity: isize, threads: u32) -> TextureLoader {
        let running = Arc::new(AtomicBool::from(true));
        //let loader_cache = HashMap::new();

        let (load_request_tx, load_request_rx) = channel();
        let load_request_rx = Arc::new(Mutex::new(load_request_rx));

        let (loaded_img_tx, loaded_img_rx) = channel();

        let mut join_handles = Vec::new();
        for _ in 0..threads {
            let mut running = running.clone();
            let mut load_request_rx = load_request_rx.clone();
            let mut loaded_img_tx = loaded_img_tx.clone();

            join_handles.push(thread::spawn(move || {
                Self::thread_loop(running, load_request_rx, loaded_img_tx);
            }));
        }

        TextureLoader {
            curr_est_size: capacity as usize,

            running,
            remaining_capacity: capacity,
            //loader_cache,
            texture_cache: HashMap::new(),
            join_handles: Some(join_handles),

            image_rx: loaded_img_rx,
            path_tx: load_request_tx,
        }
    }

    fn thread_loop(
        running: Arc<AtomicBool>,
        load_request_rx: Arc<Mutex<Receiver<PathBuf>>>,
        loaded_img_tx: Sender<(PathBuf, fs::Metadata, image::RgbaImage)>,
    ) {
        // walk the directory starting from the current item and cache in all the images
        // do this by stepping in both directions so that the cached images ahead of the file
        // should never be more than 1 + "cached images before the file"
        while running.load(Ordering::SeqCst) {
            let img_path = {
                let load_request = load_request_rx.lock().unwrap();
                if let Some(path) = load_request.recv().ok() {
                    path
                } else {
                    return;
                }
            };
            // It is very important that we release the mutex before starting to load the image

            let metadata = match fs::metadata(img_path.as_path()) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let image = match Self::load_image(img_path.as_path()) {
                Ok(image) => image,
                Err(_) => continue,
            };

            if loaded_img_tx.send((img_path, metadata, image)).is_err() {
                return;
            }
            //thread::sleep(time::Duration::from_millis(1));
        }
    }

    pub fn process_prefetched(&mut self, display: &glium::Display) -> Result<()> {
        use std::collections::hash_map::Entry;
        use std::sync::mpsc::TryRecvError;

        loop {
            match self.image_rx.try_recv() {
                Ok((path, metadata, image)) => {
                    let size_estimate =
                        Self::get_image_size_estimate((image.width(), image.height())) as isize;
                    match self.texture_cache.entry(path) {
                        Entry::Vacant(_entry) => unreachable!(),
                        Entry::Occupied(mut entry) => match entry.get_mut() {
                            CachedTexture::Texture(ref mut entry) => {
                                if entry.0.modified().unwrap() < metadata.modified().unwrap() {
                                    let old_size_estimate = {
                                        let old_image = &entry.1;
                                        Self::get_image_size_estimate((
                                            old_image.width(),
                                            old_image.height(),
                                        )) as isize
                                    };
                                    let texture =
                                        Rc::new(Self::texture_from_image(display, image)?);
                                    entry.0 = metadata;
                                    entry.1 = texture;
                                    self.remaining_capacity += old_size_estimate;
                                    self.remaining_capacity -= size_estimate;
                                }
                            }
                            entry @ CachedTexture::LoadRequested => {
                                let texture = Rc::new(Self::texture_from_image(display, image)?);
                                *entry = CachedTexture::Texture((metadata, texture));
                                self.remaining_capacity -= size_estimate;
                            }
                        },
                    }
                }
                Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => break,
            }
        }

        Ok(())
    }

    pub fn send_load_requests(&mut self, dir_files: &Vec<fs::DirEntry>, current_name: &OsString) {
        use std::collections::hash_map::Entry;

        let mut iter = dir_files.iter();

        // Step until curr file
        while let Some(entry) = iter.next() {
            if entry.file_name() == *current_name {
                break;
            }
        }

        let mut requested_images = 0;
        // Send as many load requests so that the estimated total will just fill the cache
        let mut estimated_remaining_cap = self.remaining_capacity;
        while estimated_remaining_cap > self.curr_est_size as isize {
            // Send a load request for the closest file not in the cache or outdated
            if let Some(file) = iter.next() {
                let file_path = file.path();
                match self.texture_cache.entry(file_path.clone()) {
                    Entry::Vacant(entry) => {
                        if Self::is_file_supported(file_path.as_ref()) {
                            entry.insert(CachedTexture::LoadRequested);
                            self.path_tx.send(file.path()).unwrap();
                        }
                    }
                    Entry::Occupied(entry) => {
                        if let CachedTexture::Texture(ref entry) = entry.get() {
                            if entry.0.modified().unwrap()
                                != file.metadata().unwrap().modified().unwrap()
                            {
                                self.path_tx.send(file_path).unwrap();
                            }
                        }
                    }
                }
                estimated_remaining_cap -= self.curr_est_size as isize;
                requested_images += 1;
                if requested_images >= Self::MAX_BULK_PREFETCH_REQUEST {
                    break;
                }
            } else {
                break;
            }
        }
    }

    pub fn load_specific(
        &mut self,
        display: &glium::Display,
        path: &PathBuf,
        dir_files: &Vec<fs::DirEntry>,
    ) -> Result<Rc<SrgbTexture2d>> {
        use std::collections::hash_map::Entry;

        let path = Path::new(path).canonicalize()?;
        let metadata = fs::metadata(path.as_path())?;

        // Lets just process incoming images
        self.process_prefetched(display)?;

        // Check if it is inside the texture cache first
        {
            let texture_entry = self.texture_cache.entry(path.clone());
            if let Entry::Occupied(ref entry) = texture_entry {
                if let CachedTexture::Texture(ref entry) = entry.get() {
                    if entry.0.modified().unwrap() == metadata.modified().unwrap() {
                        return Ok(entry.1.clone());
                    }
                }
            }
        }

        let image = Self::load_image(path.as_path())?;
        self.curr_est_size =
            Self::get_image_size_estimate((image.width(), image.height())) as usize;
        let image_size_estimate = self.curr_est_size as isize;

        if self.remaining_capacity < image_size_estimate {
            // Find the position of the current file in the directory
            let mut current_pos = 0;
            let loaded_filename = path.file_name().unwrap();
            for (i, file_in_dir) in dir_files.iter().enumerate() {
                if file_in_dir.file_name() == loaded_filename {
                    current_pos = i;
                    break;
                }
            }

            let mut cached_ordered: Vec<_> = dir_files
                .iter()
                .filter_map(|file| {
                    let path = file.path();
                    if self.texture_cache.contains_key(&path) {
                        Some(path)
                    } else {
                        None
                    }
                })
                .enumerate()
                .collect();

            // And there is just one more thing left to do...
            // Walk through our list of directory entries sorted by their distance from the current
            // file and in each step remove an entry from the cache until we reach the desired cache
            // size
            for (file_pos, file) in cached_ordered.into_iter() {
                if self.remaining_capacity < image_size_estimate && file_pos < current_pos {
                    if let Entry::Occupied(entry) = self.texture_cache.entry(file) {
                        let size_estimate = if let CachedTexture::Texture(ref entry) = entry.get() {
                            Some(
                                Self::get_image_size_estimate((entry.1.width(), entry.1.height()))
                                    as isize,
                            )
                        } else {
                            None
                        };
                        if let Some(size_estimate) = size_estimate {
                            entry.remove();
                            self.remaining_capacity += size_estimate;
                        }
                    }
                } else {
                    break;
                }
            }
        }
        self.remaining_capacity -= image_size_estimate;

        let result_texture = Rc::new(Self::texture_from_image(display, image)?);
        match self.texture_cache.entry(path.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(CachedTexture::Texture((metadata, result_texture.clone())));
            }
            Entry::Occupied(mut entry) => match entry.get_mut() {
                entry @ CachedTexture::LoadRequested => {
                    *entry = CachedTexture::Texture((metadata, result_texture.clone()));
                }
                _ => unreachable!(),
            },
        }

        //self.send_load_requests();

        Ok(result_texture)
    }

    fn load_iter_jump<'a, IterT>(
        &mut self,
        display: &glium::Display,
        mut entries_twice: IterT,
        jump_count: u32,
        dir_files: &Vec<fs::DirEntry>,
        current_name: &OsString,
    ) -> Result<(Rc<SrgbTexture2d>, OsString)>
    where
        IterT: iter::Iterator<Item = &'a fs::DirEntry>,
    {
        let mut jump_remaining = jump_count;
        'finding_current: while let Some(curr_file) = entries_twice.next() {
            if curr_file.file_type()?.is_file() {
                if curr_file.file_name() == *current_name {
                    // Find next file
                    'finding_target: while let Some(next) = entries_twice.next() {
                        if next.file_type().unwrap().is_file() {
                            let next_filepath = next.path();
                            if Self::is_file_supported(next_filepath.as_ref()) {
                                if jump_remaining == 0 {
                                    let next_filepath_str = next_filepath.to_str().unwrap();
                                    match self.load_specific(display, &next_filepath, dir_files) {
                                        Err(Error(ErrorKind::ImageLoadError(err), ..)) => {
                                            // The file has to be supported at this point
                                            return Err(Error::from(
                                                format!("Image file should have been supported but it is not. ('{}')", next_filepath_str),
                                            ));
                                        }
                                        Err(err) => {
                                            // Some other error occured, it is a bad sign,
                                            // just return the error
                                            return Err(err);
                                        }
                                        Ok(result) => {
                                            return Ok((
                                                result,
                                                next_filepath.file_name().unwrap().to_owned(),
                                            ));
                                        }
                                    }
                                } else {
                                    jump_remaining -= 1;
                                }
                            }
                        }
                    }

                    return Err(Error::from("Couldn't jump to the requested file."));
                }
            }
        }

        Err(Error::from(
            "Couldn't find the current file in the the directory",
        ))
    }

    ///
    /// entries_twice should be an iterator of the folder chained with itself.
    ///
    fn load_iter_next<'a, IterT>(
        &mut self,
        display: &glium::Display,
        mut entries_twice: IterT,
        dir_files: &Vec<fs::DirEntry>,
        current_name: &OsString,
    ) -> Result<(Rc<SrgbTexture2d>, OsString)>
    where
        IterT: iter::Iterator<Item = &'a fs::DirEntry>,
    {
        'finding_current: while let Some(curr_file) = entries_twice.next() {
            if curr_file.file_type()?.is_file() {
                if curr_file.file_name() == *current_name {
                    // Find next file
                    'finding_next: while let Some(next) = entries_twice.next() {
                        if next.file_type().unwrap().is_file() {
                            let next_filename = next.path();
                            match self.load_specific(display, &next_filename, dir_files) {
                                Err(Error(ErrorKind::ImageLoadError(_err), ..)) => {
                                    // Image type not supported, just skip it
                                    continue 'finding_next;
                                }
                                Err(err) => {
                                    // Some other error occured, it is a bad sign,
                                    // just return the error
                                    return Err(err);
                                }
                                Ok(result) => {
                                    return Ok((
                                        result,
                                        next_filename.file_name().unwrap().to_owned(),
                                    ));
                                }
                            }
                        }
                    }
                    // Current already found at this point
                    break 'finding_current;
                }
            }
        }

        Err(Error::from(format!(
            "Couldn't find the current file in the the directory. Current file: '{}'",
            current_name.to_str().unwrap()
        )))
    }

    fn load_image(image_path: &Path) -> Result<image::RgbaImage> {
        Ok(image::open(image_path)?.to_rgba())
    }

    /*
    fn texture_from_loader(display: &glium::Display, image: LoaderImage) -> Result<SrgbTexture2d> {
        match image {
            LoaderImage::Image(image) => {
                let result = Self::texture_from_image(display, image)?;
                Ok(result)
            }
            LoaderImage::Processed => Err(Error::from(
                "Loader image was requested to be converted but it has already been processed",
            )),
        }
    }
    */

    fn texture_from_image(
        display: &glium::Display,
        image: image::RgbaImage,
    ) -> Result<SrgbTexture2d> {
        let image_dimensions = image.dimensions();
        let image = RawImage2d::from_raw_rgba(image.into_raw(), image_dimensions);

        Ok(SrgbTexture2d::with_mipmaps(
            display,
            image,
            glium::texture::MipmapsOption::AutoGeneratedMipmapsMax(4),
        )?)
    }

    fn get_image_size_estimate(dimensions: (u32, u32)) -> u32 {
        // counting all the mipmaps would add an additionnal multiplier of around ~1.6
        // but only the gpu textures have mip maps so just multiply by 1.5
        ((dimensions.0 * dimensions.1 * 4) as f32 * 1.5) as u32
    }

    pub fn is_file_supported(filename: &Path) -> bool {
        if let Some(ext) = filename.extension() {
            if let Some(ext) = ext.to_str() {
                let ext = ext.to_lowercase();
                match ext.as_str() {
                    "png" | "jpg" | "bmp" | "gif" | "tiff" | "webp" | "pnm" => {
                        return true;
                    }
                    _ => (),
                }
            }
        }

        false
    }
}

impl Drop for TextureLoader {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        match self.join_handles.take() {
            Some(mut join_handles) => {
                for _ in join_handles.iter() {
                    self.path_tx.send(PathBuf::from("")).unwrap();
                }

                for mut handle in join_handles.into_iter() {
                    match handle.join() {
                        Err(err) => eprintln!("Error occured while joining handle {:?}", err),
                        _ => (),
                    }
                }
            }
            _ => (),
        }
    }
}

enum CachedTexture {
    Texture((fs::Metadata, Rc<SrgbTexture2d>)),
    LoadRequested,
}

pub struct ImageCache {
    dir_path: PathBuf,
    current_name: OsString,
    dir_files: Vec<fs::DirEntry>,

    loader: TextureLoader,
}

/// This is a store for the supported images loaded from a folder
/// The basic idea is to have a few images already in the memory while an image is shown on the screen
impl ImageCache {
    /// # Arguemnts
    /// * `capacity` - Number of bytes. The last image loaded will be the one at which the allocated memory reaches or exceeds capacity
    pub fn new(capacity: isize, threads: u32) -> ImageCache {
        ImageCache {
            dir_path: PathBuf::new(),
            current_name: OsString::new(),
            dir_files: Vec::new(),

            loader: TextureLoader::new(capacity, threads),
        }
    }

    pub fn update_directory(&mut self) -> Result<()> {
        self.dir_files = fs::read_dir(self.dir_path.as_path())?
            .filter_map(|x| {
                let entry = x.unwrap();
                if entry.file_type().unwrap().is_file() {
                    Some(entry)
                } else {
                    None
                }
            })
            .collect();

        self.dir_files
            .sort_unstable_by(|a, b| a.file_name().cmp(&b.file_name()));

        Ok(())
    }

    pub fn load_specific(
        &mut self,
        display: &glium::Display,
        path: &str,
    ) -> Result<Rc<SrgbTexture2d>> {
        use std::collections::hash_map::Entry;

        let path = Path::new(path).canonicalize()?;
        let metadata = fs::metadata(path.as_path())?;

        self.current_name = match path.file_name() {
            Some(filename) => filename.to_owned(),
            None => bail!(format!(
                "Could not get filename for path '{}'",
                path.to_str().unwrap()
            )),
        };

        // Directory may have changed
        let parent = path.parent().unwrap().to_owned(); // It absolutely must have a parent if it was a file
        if self.dir_path != parent {
            self.dir_path = parent;
            self.update_directory()?;
        }

        return self.loader.load_specific(display, &path, &self.dir_files);
    }

    pub fn load_next(&mut self, display: &glium::Display) -> Result<(Rc<SrgbTexture2d>, OsString)> {
        let iter = self.dir_files.iter().chain(self.dir_files.iter());
        let result = self.loader
            .load_iter_next(display, iter, &self.dir_files, &self.current_name);
        match result {
            Ok((_, ref filename)) => {
                self.current_name = filename.clone();
            }
            _ => (),
        }

        result
    }

    pub fn load_prev(&mut self, display: &glium::Display) -> Result<(Rc<SrgbTexture2d>, OsString)> {
        let iter = self.dir_files.iter().chain(self.dir_files.iter()).rev();
        let result = self.loader
            .load_iter_next(display, iter, &self.dir_files, &self.current_name);
        match result {
            Ok((_, ref filename)) => {
                self.current_name = filename.clone();
            }
            _ => (),
        }

        result
    }

    pub fn load_jump(
        &mut self,
        display: &glium::Display,
        jump_count: i32,
    ) -> Result<(Rc<SrgbTexture2d>, OsString)> {
        if jump_count == 0 {
            return Ok((
                match self.loader
                    .texture_cache
                    .get(self.dir_path.join(self.current_name.clone()).as_path())
                {
                    Some(CachedTexture::Texture(ref entry)) => entry.1.clone(),
                    _ => bail!(Error::from("Could not find current file in cache.")),
                },
                self.current_name.clone(),
            ));
        }

        let forward_iter = self.dir_files.iter().chain(self.dir_files.iter());
        let result = if jump_count < 0 {
            self.loader.load_iter_jump(
                display,
                forward_iter.rev(),
                -jump_count as u32,
                &self.dir_files,
                &self.current_name,
            )
        } else {
            self.loader.load_iter_jump(
                display,
                forward_iter,
                jump_count as u32,
                &self.dir_files,
                &self.current_name,
            )
        };

        match result {
            Ok((_, ref filename)) => {
                self.current_name = filename.clone();
            }
            _ => (),
        }

        result
    }

    pub fn process_prefetched(&mut self, display: &glium::Display) -> Result<()> {
        self.loader.process_prefetched(display)
    }

    pub fn send_load_requests(&mut self) {
        self.loader
            .send_load_requests(&self.dir_files, &self.current_name);
    }
}
